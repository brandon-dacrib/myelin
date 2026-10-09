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
 * "When changes apply", once per section: a headline, each class this section has with how many
 * settings are in it, and what each class means behind "What that means". The headline and the
 * badges are the answer; the paragraphs are for the first time, and start closed.
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
      <p className="mt-1 flex flex-wrap items-center gap-2 text-xs text-text-muted">
        {present.map((applies) => (
          <span key={applies} className="flex items-center gap-1.5">
            <AppliesBadge applies={applies} />
            {counts[applies]} {counts[applies] === 1 ? "setting" : "settings"}
          </span>
        ))}
      </p>
      <details className="mt-2">
        <summary className="cursor-pointer text-xs text-accent hover:underline">
          What that means
        </summary>
        <dl className="mt-2 flex flex-col gap-2">
          {present.map((applies) => (
            <div
              key={applies}
              className="grid gap-x-4 gap-y-1 sm:grid-cols-[minmax(0,15rem)_minmax(0,1fr)]"
            >
              <dt className="text-sm font-medium text-text">{APPLIES_COPY[applies].label}:</dt>
              <dd className="text-sm text-text-muted">{APPLIES_COPY[applies].explanation}</dd>
            </div>
          ))}
        </dl>
      </details>
    </section>
  );
}
