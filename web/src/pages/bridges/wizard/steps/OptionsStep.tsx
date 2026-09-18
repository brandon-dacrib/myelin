import { Root, Thumb } from "radix-ui/switch";
import { cn } from "@/lib/cn";
import type { WizardFormState } from "../wizard-state";

function Toggle({
  label,
  description,
  checked,
  onChange,
}: {
  label: string;
  description: string;
  checked: boolean;
  onChange: (checked: boolean) => void;
}) {
  return (
    <div className="flex items-start justify-between gap-4 rounded-md border border-border bg-surface p-4">
      <div>
        <p className="text-sm font-medium text-text">{label}</p>
        <p className="mt-0.5 text-sm text-text-muted">{description}</p>
      </div>
      <Root
        checked={checked}
        onCheckedChange={onChange}
        aria-label={label}
        className={cn(
          "relative h-6 w-10 shrink-0 rounded-full transition-colors duration-fast",
          checked ? "bg-accent" : "bg-border-strong",
          "focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--color-focus)]",
        )}
      >
        <Thumb
          className={cn(
            "block size-5 translate-x-0.5 rounded-full bg-surface transition-transform duration-fast",
            "data-[state=checked]:translate-x-[18px]",
          )}
        />
      </Root>
    </div>
  );
}

export function OptionsStep({
  state,
  onChange,
}: {
  state: WizardFormState;
  onChange: (patch: Partial<WizardFormState>) => void;
}) {
  return (
    <div>
      <h2 className="text-lg text-text">Options</h2>
      <p className="mt-1 text-sm text-text-muted">All on by default; each does one clear thing.</p>

      <div className="mt-6 flex flex-col gap-3">
        <Toggle
          label="Double puppeting"
          description="Creates or reuses the shared non-exclusive registration and tells the bridge about it."
          checked={state.doublePuppeting}
          onChange={(v) => onChange({ doublePuppeting: v })}
        />
        <Toggle
          label="Encryption"
          description="MSC2409 ephemeral, MSC3202 device lists and OTK counts, MSC4190 device management."
          checked={state.encryption}
          onChange={(v) => onChange({ encryption: v })}
        />
        <Toggle
          label="Rate-limit exemption"
          description="The bridge bot and its puppets are exempt from the homeserver's rate limits."
          checked={state.rateLimitExempt}
          onChange={(v) => onChange({ rateLimitExempt: v })}
        />
      </div>
    </div>
  );
}
