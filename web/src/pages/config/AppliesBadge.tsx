/**
 * When a change to a setting takes effect: one badge per setting, and a legend at the top of a
 * section that explains each class present there once (`lib/config-applies.ts`).
 */
import { Badge } from "@/components/ui/badge/Badge";
import { APPLIES_COPY, APPLIES_ORDER, appliesHeadline, type Applies } from "@/lib/config-applies";

export function AppliesBadge({ applies }: { applies: Applies }) {
  const copy = APPLIES_COPY[applies];
  return (
    <Badge status={copy.status} hideIcon={applies !== "hot"}>
      {copy.label}
    </Badge>
  );
}

/**
 * "When changes apply", once per section: a headline, then each class this section has, with
 * how many of its settings are in it and what the class means.
 */
export function AppliesLegend({ counts }: { counts: Record<Applies, number> }) {
  const present = APPLIES_ORDER.filter((a) => counts[a] > 0);
  if (present.length === 0) return null;
  return (
    <section
      aria-labelledby="applies-legend-heading"
      className="mt-4 rounded-md border border-border bg-surface-sunken p-4"
    >
      <h2 id="applies-legend-heading" className="text-sm font-medium text-text">
        {appliesHeadline(counts)}
      </h2>
      <dl className="mt-2 flex flex-col gap-2">
        {present.map((applies) => (
          <div
            key={applies}
            className="grid gap-x-4 gap-y-1 sm:grid-cols-[minmax(0,15rem)_minmax(0,1fr)]"
          >
            <dt className="flex flex-wrap items-center gap-2 text-sm text-text">
              <AppliesBadge applies={applies} />
              <span className="text-xs text-text-muted">
                {counts[applies]} {counts[applies] === 1 ? "setting" : "settings"}
              </span>
            </dt>
            <dd className="text-sm text-text-muted">{APPLIES_COPY[applies].explanation}</dd>
          </div>
        ))}
      </dl>
    </section>
  );
}
