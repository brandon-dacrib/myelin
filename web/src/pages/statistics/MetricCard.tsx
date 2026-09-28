import { isCounter, RANGES, useTimeseries, type Metric, type RangeId } from "@/api/statistics";
import { QueryProblemState } from "@/components/QueryProblemState";
import { TimeseriesChart } from "@/components/TimeseriesChart";
import { Skeleton } from "@/components/ui/skeleton/Skeleton";

export interface MetricCardProps {
  metric: Metric;
  title: string;
  range: RangeId;
  formatValue: (value: number) => string;
  /** One sentence under the title saying what is counted. */
  description: string;
}

/**
 * One metric over the chosen range, with its headline number: the total over the range for a
 * counter, the latest sample for a gauge.
 */
export function MetricCard({ metric, title, range, formatValue, description }: MetricCardProps) {
  const query = useTimeseries(metric, range);
  const counter = isCounter(metric);
  const points = query.data?.points ?? [];
  const values = points.map((p) => p.value ?? 0);
  const headline = counter
    ? formatValue(values.reduce((a, b) => a + b, 0))
    : values.length
      ? formatValue(values[values.length - 1])
      : "—";
  const headingId = `metric-${metric.replace(/\W/g, "-")}`;
  const until = query.dataUpdatedAt;

  return (
    <section
      aria-labelledby={headingId}
      className="flex flex-col gap-3 rounded-md border border-border bg-surface p-4"
    >
      <div className="flex flex-wrap items-baseline justify-between gap-2">
        <div>
          <h3 id={headingId} className="text-md font-medium text-text">
            {title}
          </h3>
          <p className="text-xs text-text-muted">{description}</p>
        </div>
        {!query.isError && !query.isLoading && (
          <p className="text-right">
            <span className="block text-2xl text-text tabular-nums">{headline}</span>
            <span className="block text-xs text-text-muted">
              {counter ? RANGES[range].label.toLowerCase() : "latest"}
            </span>
          </p>
        )}
      </div>
      {query.isLoading ? (
        <Skeleton className="h-40 rounded-sm" />
      ) : query.isError ? (
        <QueryProblemState
          error={query.error}
          resource={title.toLowerCase()}
          scope="admin:read"
          compact
          onRetry={() => query.refetch()}
        />
      ) : (
        <TimeseriesChart
          label={`${title}, ${RANGES[range].label.toLowerCase()}`}
          points={points.map((p) => ({ at: p.at ?? "", value: p.value ?? 0 }))}
          stepMs={query.data?.step_ms ?? 3_600_000}
          domain={{ from: until - RANGES[range].ms, until }}
          variant={counter ? "bars" : "line"}
          formatValue={formatValue}
        />
      )}
      {!counter && !query.isLoading && !query.isError && (
        <p className="text-xs text-text-faint">
          Sampled by the server every 15 minutes; history starts when sampling did.
        </p>
      )}
    </section>
  );
}
