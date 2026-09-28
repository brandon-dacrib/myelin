/**
 * The controls the registration-token dialogs share: a short radio choice, the "uses allowed"
 * number with its Unlimited switch, the expiry choice, and the invite link to hand over. Each is
 * a real control (decision 0010): nothing about a token is typed as JSON.
 */
import { useId, useState, type ReactNode } from "react";
import { Check, Copy } from "lucide-react";
import { Button } from "@/components/ui/button/Button";
import { Field, Input } from "@/components/ui/input/Input";
import { Switch } from "@/components/ui/switch/Switch";
import { EXPIRY_PRESETS, type ExpiryChoice } from "@/lib/registration-tokens";
import { cn } from "@/lib/cn";

export interface Choice<T extends string> {
  value: T;
  label: ReactNode;
}

/**
 * A handful of mutually exclusive options as native radios in a fieldset, laid out as a row of
 * segments: a screen reader hears "Expires, radio group", and every option is visible at once.
 */
export function ChoiceGroup<T extends string>({
  legend,
  name,
  value,
  onChange,
  choices,
  hint,
}: {
  legend: string;
  name: string;
  value: T;
  onChange: (value: T) => void;
  choices: Choice<T>[];
  hint?: ReactNode;
}) {
  const hintId = useId();
  return (
    <fieldset className="flex flex-col gap-1.5" aria-describedby={hint ? hintId : undefined}>
      <legend className="mb-1.5 text-sm font-medium text-text">{legend}</legend>
      <div className="flex flex-wrap gap-1 rounded-sm border border-border-strong bg-surface p-1">
        {choices.map((choice) => (
          <label
            key={choice.value}
            className={cn(
              "flex cursor-pointer items-center gap-2 rounded-xs px-2.5 py-1 text-sm text-text",
              "has-[:checked]:bg-accent-muted has-[:checked]:text-accent hover:bg-surface-sunken",
            )}
          >
            <input
              type="radio"
              name={name}
              value={choice.value}
              checked={value === choice.value}
              onChange={() => onChange(choice.value)}
              className="accent-accent focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--color-focus)]"
            />
            {choice.label}
          </label>
        ))}
      </div>
      {hint && (
        <p id={hintId} className="text-xs text-text-muted">
          {hint}
        </p>
      )}
    </fieldset>
  );
}

/** How many accounts a token may create: a whole number, or no limit at all. */
export function UsesControl({
  uses,
  onUsesChange,
  unlimited,
  onUnlimitedChange,
  error,
}: {
  uses: string;
  onUsesChange: (uses: string) => void;
  unlimited: boolean;
  onUnlimitedChange: (unlimited: boolean) => void;
  error?: string;
}) {
  const switchLabelId = useId();
  return (
    <Field
      label="Uses allowed"
      hint="How many accounts can be created with it."
      error={error}
      required={!unlimited}
    >
      {(fieldProps) => (
        <div className="flex items-center gap-4">
          <Input
            {...fieldProps}
            type="number"
            inputMode="numeric"
            min={0}
            step={1}
            className="max-w-28"
            disabled={unlimited}
            value={unlimited ? "" : uses}
            onChange={(e) => onUsesChange(e.target.value)}
          />
          <span className="flex items-center gap-2">
            <Switch
              checked={unlimited}
              onCheckedChange={onUnlimitedChange}
              aria-labelledby={switchLabelId}
            />
            <span id={switchLabelId} className="text-sm text-text">
              Unlimited
            </span>
          </span>
        </div>
      )}
    </Field>
  );
}

/** When a token stops working: never, a preset from now, or a chosen date and time. */
export function ExpiryControl({
  choice,
  onChoiceChange,
  custom,
  onCustomChange,
  error,
  presets = true,
}: {
  choice: ExpiryChoice;
  onChoiceChange: (choice: ExpiryChoice) => void;
  custom: string;
  onCustomChange: (custom: string) => void;
  error?: string;
  /** The create dialog offers "in 1 day" and friends; the edit dialog offers only never or a date. */
  presets?: boolean;
}) {
  const choices: Choice<ExpiryChoice>[] = [
    { value: "never", label: "Never" },
    ...(presets ? EXPIRY_PRESETS.map((p) => ({ value: p.id, label: p.label })) : []),
    { value: "custom", label: "Date and time" },
  ];
  return (
    <div className="flex flex-col gap-3">
      <ChoiceGroup
        legend="Expires"
        name="expiry"
        value={choice}
        onChange={onChoiceChange}
        choices={choices}
      />
      {choice === "custom" && (
        <Field label="Expires at" hint="In this browser's time zone." error={error} required>
          {(fieldProps) => (
            <Input
              {...fieldProps}
              type="datetime-local"
              className="max-w-64"
              value={custom}
              onChange={(e) => onCustomChange(e.target.value)}
            />
          )}
        </Field>
      )}
    </div>
  );
}

/**
 * The invite link, large enough to read and with one button to copy it: what the
 * administrator came for when they made a token.
 */
export function InviteLinkPanel({ link }: { link: string }) {
  const [copied, setCopied] = useState(false);
  async function handleCopy() {
    try {
      await navigator.clipboard.writeText(link);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      /* clipboard unavailable: the link stays visible and selectable */
    }
  }

  return (
    <div className="rounded-md border border-accent bg-accent-muted p-4">
      <p className="text-sm font-medium text-text">Invite link</p>
      <p className="mt-1 break-all font-identifier text-base text-text">{link}</p>
      <Button
        className="mt-3"
        size="sm"
        onClick={handleCopy}
        leadingIcon={
          copied ? <Check size={14} aria-hidden="true" /> : <Copy size={14} aria-hidden="true" />
        }
      >
        {copied ? "Copied" : "Copy invite link"}
      </Button>
    </div>
  );
}

/** Copies a token's invite link from a table row, saying so on the button for a moment. */
export function CopyInviteLinkButton({ link, token }: { link: string; token: string }) {
  const [copied, setCopied] = useState(false);
  async function handleCopy() {
    try {
      await navigator.clipboard.writeText(link);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      /* clipboard unavailable */
    }
  }
  return (
    <Button
      variant="ghost"
      size="sm"
      onClick={handleCopy}
      aria-label={copied ? "Copied" : `Copy invite link for ${token}`}
      leadingIcon={
        copied ? <Check size={14} aria-hidden="true" /> : <Copy size={14} aria-hidden="true" />
      }
    >
      {copied ? "Copied" : "Copy invite link"}
    </Button>
  );
}
