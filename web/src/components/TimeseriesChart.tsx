import { useId, useMemo, useState, type PointerEvent } from "react";
import { cn } from "@/lib/cn";

export interface ChartPoint {
  at: string;
  value: number;
}

export interface TimeseriesChartProps {
  /** What the series is, for the accessible name and the table's caption ("New accounts"). */
  label: string;
  points: ChartPoint[];
  /** The width of one step, in milliseconds (the series' `step_ms`). */
  stepMs: number;
  /** The window asked for; the x axis spans it even where the series has no points. */
  domain: { from: number; until: number };
  /**
   * `bars` for a counter (what happened in each step), `line` for a gauge (a sampled level,
   * drawn only between the samples it has).
   */
  variant: "bars" | "line";
  formatValue: (value: number) => string;
  className?: string;
}

const W = 1000;
const H = 200;

function formatAt(at: number, stepMs: number): string {
  const date = new Date(at);
  return stepMs >= 86_400_000
    ? date.toLocaleDateString(undefined, { month: "short", day: "numeric" })
    : date.toLocaleString(undefined, {
        month: "short",
        day: "numeric",
        hour: "2-digit",
        minute: "2-digit",
      });
}

/** A round-ish top for the y axis, so the one gridline label reads as a number people use. */
function niceMax(max: number): number {
  if (max <= 0) return 1;
  const magnitude = 10 ** Math.floor(Math.log10(max));
  for (const m of [1, 1.2, 1.5, 2, 2.5, 3, 4, 5, 6, 8, 10]) {
    if (m * magnitude >= max) return m * magnitude;
  }
  return 10 * magnitude;
}

/**
 * One metric over time: bars for a counter, a line for a gauge, one axis, a recessive grid, a
 * crosshair and tooltip on hover, and the same numbers as a table for anyone who would rather
 * read them (or cannot see the chart). Single series, so no legend: the heading names it.
 */
export function TimeseriesChart({
  label,
  points,
  stepMs,
  domain,
  variant,
  formatValue,
  className,
}: TimeseriesChartProps) {
  const [active, setActive] = useState<number | null>(null);
  const [showTable, setShowTable] = useState(false);
  const tableId = useId();

  const parsed = useMemo(
    () => points.map((p) => ({ at: Date.parse(p.at), value: p.value })),
    [points],
  );
  const from = Math.min(domain.from, parsed[0]?.at ?? domain.from);
  const until = Math.max(domain.until, (parsed.at(-1)?.at ?? domain.until) + stepMs);
  const span = Math.max(until - from, 1);
  const rawTop = niceMax(Math.max(0, ...parsed.map((p) => p.value)));
  // Whole-number series (counts of things) keep whole-number gridlines: no "1.5 accounts".
  const whole = parsed.every((p) => Number.isInteger(p.value));
  const top = whole && rawTop % 2 !== 0 ? Math.ceil(rawTop / 2) * 2 : rawTop;
  const x = (at: number) => ((at - from) / span) * W;
  const y = (value: number) => H - (value / top) * H;
  const barWidth = Math.max((stepMs / span) * W - 2, 1);

  // A gap in the samples (a step with no point) breaks the line rather than bridging it.
  const linePath = parsed
    .map((p, i) => {
      const joined = i > 0 && p.at - parsed[i - 1].at <= stepMs * 1.5;
      return `${joined ? "L" : "M"} ${x(p.at + stepMs / 2).toFixed(1)} ${y(p.value).toFixed(1)}`;
    })
    .join(" ");

  function onPointerMove(event: PointerEvent<HTMLDivElement>) {
    if (parsed.length === 0) return;
    const rect = event.currentTarget.getBoundingClientRect();
    const at = from + ((event.clientX - rect.left) / rect.width) * span;
    let nearest = 0;
    for (let i = 1; i < parsed.length; i++) {
      if (Math.abs(parsed[i].at + stepMs / 2 - at) < Math.abs(parsed[nearest].at + stepMs / 2 - at))
        nearest = i;
    }
    setActive(nearest);
  }

  const activePoint = active != null ? parsed[active] : undefined;
  const activeLeft = activePoint ? (x(activePoint.at + stepMs / 2) / W) * 100 : 0;
  const latest = parsed.at(-1);
  const summary =
    parsed.length === 0
      ? `${label}: no data in this range.`
      : `${label}: ${parsed.length} points; highest ${formatValue(Math.max(...parsed.map((p) => p.value)))}; latest ${formatValue(latest!.value)}.`;

  return (
    <div className={cn("flex flex-col gap-2", className)}>
      <div className="flex gap-2">
        {/* y-axis labels: text in text tokens, never the series colour */}
        <div
          aria-hidden="true"
          className="flex w-16 shrink-0 flex-col justify-between text-right text-xs text-text-muted"
          style={{ height: "10rem" }}
        >
          <span className="-translate-y-1/2">{formatValue(top)}</span>
          <span className="-translate-y-1/2">{formatValue(top / 2)}</span>
          <span className="translate-y-1/2">0</span>
        </div>
        <div
          role="img"
          aria-label={summary}
          className="relative h-40 flex-1 touch-none"
          onPointerMove={onPointerMove}
          onPointerLeave={() => setActive(null)}
        >
          <svg
            viewBox={`0 0 ${W} ${H}`}
            preserveAspectRatio="none"
            className="absolute inset-0 h-full w-full overflow-visible"
            aria-hidden="true"
          >
            {[0, 0.5, 1].map((f) => (
              <line
                key={f}
                x1={0}
                x2={W}
                y1={H * f}
                y2={H * f}
                stroke="var(--color-border)"
                strokeWidth={1}
                vectorEffect="non-scaling-stroke"
              />
            ))}
            {variant === "bars" ? (
              parsed.map((p, i) => (
                <rect
                  key={p.at}
                  x={x(p.at) + 1}
                  y={y(p.value)}
                  width={barWidth}
                  height={H - y(p.value)}
                  fill="var(--color-accent)"
                  opacity={active == null || active === i ? 1 : 0.55}
                />
              ))
            ) : (
              <path
                d={linePath}
                fill="none"
                stroke="var(--color-accent)"
                strokeWidth={2}
                strokeLinejoin="round"
                strokeLinecap="round"
                vectorEffect="non-scaling-stroke"
              />
            )}
          </svg>
          {parsed.length === 1 && variant === "line" && (
            <span
              aria-hidden="true"
              className="absolute size-2 -translate-x-1/2 -translate-y-1/2 rounded-full bg-accent"
              style={{
                left: `${(x(parsed[0].at + stepMs / 2) / W) * 100}%`,
                top: `${(y(parsed[0].value) / H) * 100}%`,
              }}
            />
          )}
          {activePoint && (
            <>
              <span
                aria-hidden="true"
                className="pointer-events-none absolute inset-y-0 w-px bg-border-strong"
                style={{ left: `${activeLeft}%` }}
              />
              {variant === "line" && (
                <span
                  aria-hidden="true"
                  className="pointer-events-none absolute size-2.5 -translate-x-1/2 -translate-y-1/2 rounded-full border-2 border-surface bg-accent"
                  style={{ left: `${activeLeft}%`, top: `${(y(activePoint.value) / H) * 100}%` }}
                />
              )}
              <div
                role="tooltip"
                className={cn(
                  "pointer-events-none absolute top-0 z-10 whitespace-nowrap rounded-sm border border-border bg-surface-raised px-2 py-1 text-xs shadow-2",
                  activeLeft > 60 ? "-translate-x-[calc(100%+8px)]" : "translate-x-2",
                )}
                style={{ left: `${activeLeft}%` }}
              >
                <p className="text-text-muted">{formatAt(activePoint.at, stepMs)}</p>
                <p className="font-medium text-text">{formatValue(activePoint.value)}</p>
              </div>
            </>
          )}
          {parsed.length === 0 && (
            <p className="absolute inset-0 flex items-center justify-center px-4 text-center text-sm text-text-muted">
              No data in this range yet.
            </p>
          )}
        </div>
      </div>
      <div className="flex items-center justify-between gap-2 pl-18 text-xs text-text-muted">
        <span aria-hidden="true">{formatAt(from, stepMs)}</span>
        <button
          type="button"
          aria-expanded={showTable}
          aria-controls={tableId}
          onClick={() => setShowTable((v) => !v)}
          className="rounded-sm px-1 text-accent hover:underline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--color-focus)]"
        >
          {showTable ? "Hide table" : "Show as table"}
        </button>
        <span aria-hidden="true">{formatAt(until, stepMs)}</span>
      </div>
      {showTable && (
        // A scrolling region has to be reachable by keyboard to be scrolled by it.
        <div
          id={tableId}
          role="region"
          aria-label={`${label} as a table`}
          // axe's scrollable-region-focusable: a keyboard user scrolls it by focusing it.
          // eslint-disable-next-line jsx-a11y/no-noninteractive-tabindex
          tabIndex={0}
          className="max-h-64 overflow-y-auto rounded-sm border border-border focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--color-focus)]"
        >
          <table className="w-full text-sm">
            <caption className="sr-only">{label}</caption>
            <thead className="sticky top-0 bg-surface-sunken text-left text-xs text-text-muted">
              <tr>
                <th scope="col" className="px-3 py-1.5 font-medium">
                  From
                </th>
                <th scope="col" className="px-3 py-1.5 text-right font-medium">
                  Value
                </th>
              </tr>
            </thead>
            <tbody className="divide-y divide-border">
              {parsed.map((p) => (
                <tr key={p.at}>
                  <td className="px-3 py-1 text-text-muted">{formatAt(p.at, stepMs)}</td>
                  <td className="px-3 py-1 text-right text-text">{formatValue(p.value)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}
