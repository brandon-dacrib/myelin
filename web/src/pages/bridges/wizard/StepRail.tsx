import { Check } from "lucide-react";
import { cn } from "@/lib/cn";
import { WIZARD_STEPS, type WizardStep } from "./wizard-state";

const LABELS: Record<WizardStep, string> = {
  kind: "Kind",
  identity: "Identity",
  deployment: "Deployment",
  options: "Options",
  review: "Review",
};

export function StepRail({
  current,
  furthestAllowed,
  onSelect,
}: {
  current: WizardStep;
  furthestAllowed: WizardStep;
  onSelect: (step: WizardStep) => void;
}) {
  const currentIndex = WIZARD_STEPS.indexOf(current);
  const furthestIndex = WIZARD_STEPS.indexOf(furthestAllowed);

  return (
    <nav aria-label="Add bridge steps" className="flex flex-row gap-2 lg:flex-col lg:gap-1">
      {WIZARD_STEPS.map((step, i) => {
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
            {LABELS[step]}
          </button>
        );
      })}
    </nav>
  );
}
