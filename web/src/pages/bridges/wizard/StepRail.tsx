import { Check } from "lucide-react";
import { cn } from "@/lib/cn";

/**
 * A wizard's steps, as a rail beside it (a row on narrow screens). A step is reachable once the
 * operator has been there; the ones after the furthest reached are shown but disabled.
 */
export function StepRail<S extends string>({
  steps,
  labels,
  label,
  current,
  furthestAllowed,
  onSelect,
}: {
  steps: readonly S[];
  labels: Record<S, string>;
  /** The navigation landmark's name: "Offer a bridge steps". */
  label: string;
  current: S;
  furthestAllowed: S;
  onSelect: (step: S) => void;
}) {
  const currentIndex = steps.indexOf(current);
  const furthestIndex = steps.indexOf(furthestAllowed);

  return (
    <nav aria-label={label} className="flex flex-row flex-wrap gap-2 lg:flex-col lg:gap-1">
      {steps.map((step, i) => {
        const done = i < currentIndex;
        const active = step === current;
        const reachable = i <= furthestIndex;
        return (
          <button
            key={step}
            type="button"
            disabled={!reachable}
            aria-current={active ? "step" : undefined}
            onClick={() => reachable && onSelect(step)}
            className={cn(
              "flex items-center gap-2 rounded-sm px-3 py-2 text-left text-sm",
              active ? "bg-accent-muted text-accent font-medium" : "text-text-muted",
              reachable && !active && "hover:bg-surface-sunken hover:text-text",
              !reachable && "opacity-40",
              "focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--color-focus)]",
            )}
          >
            <span
              className={cn(
                "flex size-5 shrink-0 items-center justify-center rounded-full border text-xs",
                done ? "border-success bg-success text-canvas" : "border-border-strong",
                active && "border-accent",
              )}
            >
              {done ? <Check size={12} aria-hidden="true" /> : i + 1}
            </span>
            {labels[step]}
          </button>
        );
      })}
    </nav>
  );
}
