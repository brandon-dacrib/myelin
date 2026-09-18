/** Relative under 7 days, absolute otherwise, ISO 8601 in the tooltip (states-density-responsiveness.md #6). */
export function RelativeTime({ at }: { at: string | null | undefined }) {
  if (!at) return <span className="text-text-faint">Never</span>;
  const date = new Date(at);
  // Date.now() is intentionally impure: a relative-time label must read the
  // current time at render. This component re-renders on every table/page
  // refetch (>=15-30s cadence), which is a fine-grained enough clock for
  // "4 min ago" style copy; it does not need its own ticking interval.
  // eslint-disable-next-line react-hooks/purity -- see comment above
  const diffMs = Date.now() - date.getTime();
  const label = formatRelative(diffMs, date);
  return (
    <time dateTime={at} title={date.toISOString()} className="tabular-nums">
      {label}
    </time>
  );
}

function formatRelative(diffMs: number, date: Date): string {
  const diffSec = Math.round(diffMs / 1000);
  const sevenDaysSec = 7 * 24 * 3600;
  if (diffSec < sevenDaysSec) {
    if (diffSec < 60) return "just now";
    if (diffSec < 3600) return `${Math.round(diffSec / 60)} min ago`;
    if (diffSec < 86_400) return `${Math.round(diffSec / 3600)} h ago`;
    return `${Math.round(diffSec / 86_400)} d ago`;
  }
  return date.toLocaleDateString(undefined, { year: "numeric", month: "short", day: "numeric" });
}
