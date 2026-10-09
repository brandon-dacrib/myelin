import { useServerHealth } from "@/api/dashboard";
import { Badge } from "@/components/ui/badge/Badge";
import { QueryProblemState } from "@/components/QueryProblemState";
import { SkeletonText } from "@/components/ui/skeleton/Skeleton";
import {
  checkLabel,
  checkMeaning,
  checkStatus,
  checkWord,
  healthSummary,
} from "@/lib/server-health";

/**
 * The server's own probe summary (`GET /server/health`) on the Overview: the overall state and a
 * sentence saying what it means, with one row per check behind "Show the checks". When every
 * probe is ok the rows say nothing the sentence did not, so they start closed; when one is not,
 * they start open, because that is what the operator came to read. A check the server reports
 * `unknown` is shown as such, with why: it is what the server cannot vouch for, not a fault.
 */
export function ServerHealthCard() {
  const health = useServerHealth();
  const checks = Object.entries(health.data?.checks ?? {});
  const allOk = health.data?.status === "ok";
  return (
    <div className="rounded-md border border-border bg-surface p-4">
      <div className="flex flex-wrap items-center gap-2">
        <h3 className="text-sm font-medium text-text">Server health</h3>
        {health.data && (
          <Badge status={checkStatus(health.data.status)}>{checkWord(health.data.status)}</Badge>
        )}
      </div>
      {health.isLoading && (
        <div className="mt-3">
          <SkeletonText lines={2} />
        </div>
      )}
      {health.isError && (
        <QueryProblemState
          error={health.error}
          resource="server health"
          compact
          className="mt-3"
          onRetry={() => health.refetch()}
        />
      )}
      {health.data && (
        <>
          <p className="mt-2 text-sm text-text">
            {healthSummary(health.data.status, health.data.checks)}
          </p>
          {checks.length > 0 && (
            <details className="mt-2" open={!allOk}>
              <summary className="cursor-pointer text-xs text-accent hover:underline">
                {allOk ? "Show the checks" : "The checks"}
              </summary>
              <dl className="mt-3 grid grid-cols-1 gap-x-8 gap-y-2 sm:grid-cols-2 xl:grid-cols-3">
                {checks.map(([key, state]) => (
                  <div key={key}>
                    <dt className="flex items-center gap-2 text-sm text-text">
                      <Badge status={checkStatus(state)} className="shrink-0">
                        {checkWord(state)}
                      </Badge>
                      <span>{checkLabel(key)}</span>
                    </dt>
                    <dd className="mt-0.5 text-xs text-text-muted">{checkMeaning(state)}</dd>
                  </div>
                ))}
              </dl>
              <p className="mt-2 text-xs text-text-faint">
                What this server can vouch for right now, probe by probe; read again every 30
                seconds.
              </p>
            </details>
          )}
        </>
      )}
    </div>
  );
}
