import { useState } from "react";
import { Field, Input } from "@/components/ui/input/Input";
import { checkAppserviceIdAvailable } from "@/api/bridges";
import type { WizardFormState } from "../wizard-state";

export interface IdentityConflict {
  message: string;
  linkLabel?: string;
  linkHref?: string;
}

export function IdentityStep({
  state,
  onChange,
  conflict,
  onClearConflict,
}: {
  state: WizardFormState;
  onChange: (patch: Partial<WizardFormState>) => void;
  conflict: IdentityConflict | null;
  onClearConflict: () => void;
}) {
  const [idAvailability, setIdAvailability] = useState<
    "unknown" | "checking" | "available" | "taken"
  >("unknown");

  async function checkId(id: string) {
    if (!id) return;
    setIdAvailability("checking");
    try {
      const available = await checkAppserviceIdAvailable(id);
      setIdAvailability(available ? "available" : "taken");
    } catch {
      setIdAvailability("unknown");
    }
  }

  return (
    <div>
      <h2 className="text-lg text-text">Identity</h2>
      <p className="mt-1 text-sm text-text-muted">
        How this bridge identifies itself to the homeserver and the operator.
      </p>

      {conflict && (
        <div
          role="alert"
          className="mt-4 rounded-md border border-danger-border bg-danger-bg p-3 text-sm text-danger"
        >
          {conflict.message}
          {conflict.linkHref && (
            <>
              {" "}
              <a href={conflict.linkHref} className="underline">
                {conflict.linkLabel ?? "View"}
              </a>
            </>
          )}
        </div>
      )}

      <div className="mt-6 grid grid-cols-1 gap-4 sm:grid-cols-2">
        <Field label="Display name" required>
          {(f) => (
            <Input {...f} value={state.name} onChange={(e) => onChange({ name: e.target.value })} />
          )}
        </Field>
        <Field
          label="Appservice ID"
          required
          hint={
            idAvailability === "checking"
              ? "Checking..."
              : idAvailability === "available"
                ? "Available"
                : idAvailability === "taken"
                  ? undefined
                  : "Lowercase letters, digits and hyphens."
          }
          error={idAvailability === "taken" ? "This ID is already registered." : undefined}
        >
          {(f) => (
            <Input
              {...f}
              value={state.id}
              onChange={(e) => {
                onChange({ id: e.target.value });
                onClearConflict();
                setIdAvailability("unknown");
              }}
              onBlur={(e) => checkId(e.target.value)}
            />
          )}
        </Field>
        <Field label="Sender localpart" required hint="The bridge bot's Matrix user.">
          {(f) => (
            <Input
              {...f}
              value={state.senderLocalpart}
              onChange={(e) => onChange({ senderLocalpart: e.target.value })}
            />
          )}
        </Field>
        <Field label="User namespace" hint="Regex matching puppet users this bridge owns.">
          {(f) => (
            <Input
              {...f}
              value={state.userNamespace}
              onChange={(e) => {
                onChange({ userNamespace: e.target.value });
                onClearConflict();
              }}
            />
          )}
        </Field>
        <Field label="Alias namespace">
          {(f) => (
            <Input
              {...f}
              value={state.aliasNamespace}
              onChange={(e) => onChange({ aliasNamespace: e.target.value })}
            />
          )}
        </Field>
        <Field label="Room namespace" hint="Optional.">
          {(f) => (
            <Input
              {...f}
              value={state.roomNamespace}
              onChange={(e) => onChange({ roomNamespace: e.target.value })}
            />
          )}
        </Field>
      </div>
    </div>
  );
}
