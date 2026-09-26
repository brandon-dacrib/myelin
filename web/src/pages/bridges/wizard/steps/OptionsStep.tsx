import { Root, Thumb } from "radix-ui/switch";
import { Field, Input } from "@/components/ui/input/Input";
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

/**
 * flows.md flow 1 step 5. Each toggle says what it actually does to the registration and the
 * bridge's config -- not what an operator might hope it does.
 */
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
          description="What you send from any Matrix client appears on the other network as you, and what the bridge posts here is posted as you. The registration gets a non-exclusive claim on every local user and the bridge's config the matching secret."
          checked={state.doublePuppeting}
          onChange={(v) => onChange({ doublePuppeting: v })}
        />
        <Toggle
          label="Encryption"
          description="Encrypted rooms stay encrypted through the bridge. The registration asks for device lists and to-device messages in its transactions (MSC3202, MSC4203) and a device made without a login (MSC4190); the bridge's config turns end-to-bridge encryption on."
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

      <div className="mt-6 max-w-lg">
        <Field
          label="Bridge administrator"
          hint="The Matrix user the bridge takes admin commands from. Everyone else on this server can sign in and chat; nobody from another server can."
        >
          {(f) => (
            <Input
              {...f}
              value={state.adminUser}
              placeholder="@you:example.org"
              onChange={(e) => onChange({ adminUser: e.target.value })}
              className="font-identifier"
            />
          )}
        </Field>
      </div>
    </div>
  );
}
