import { useId, type ReactNode } from "react";
import { Cloud, Laptop } from "lucide-react";
import type { BridgeDeploymentTarget, BridgeOfferingRuntime, BridgeType } from "@/api/bridges";
import { Field, Input, Textarea } from "@/components/ui/input/Input";
import { Switch } from "@/components/ui/switch/Switch";
import { looksLikeUserId, notDeployableReason, parseUserList } from "@/lib/bridge-offerings";
import { cn } from "@/lib/cn";

/**
 * The parts of an offering an administrator chooses (RFC 0017 section 5, `BridgeOfferingRequest`),
 * shared by the Offer-a-bridge wizard's steps and the offering page's settings dialog so both
 * say the same thing the same way.
 */

/** A selectable card in a `radiogroup`. Disabled cards say why in their own text. */
export function ChoiceCard({
  selected,
  onSelect,
  title,
  description,
  icon,
  disabled,
  note,
}: {
  selected: boolean;
  onSelect: () => void;
  title: string;
  description: ReactNode;
  icon?: ReactNode;
  disabled?: boolean;
  note?: ReactNode;
}) {
  return (
    <button
      type="button"
      role="radio"
      aria-checked={selected}
      disabled={disabled}
      onClick={onSelect}
      className={cn(
        "flex items-start gap-3 rounded-md border p-4 text-left",
        selected
          ? "border-accent bg-accent-muted"
          : "border-border bg-surface enabled:hover:bg-surface-sunken",
        "disabled:cursor-not-allowed disabled:bg-surface-sunken",
        "focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--color-focus)]",
      )}
    >
      {icon && <span className="mt-0.5 shrink-0 text-text-muted">{icon}</span>}
      <span className="flex min-w-0 flex-col gap-1">
        <span className={cn("text-sm font-medium", disabled ? "text-text-muted" : "text-text")}>
          {title}
        </span>
        <span className="text-xs text-text-muted">{description}</span>
        {note && <span className="text-xs text-text-muted">{note}</span>}
      </span>
    </button>
  );
}

/** Who may have an instance: everyone local, or the people listed. */
export function AccessFields({
  allLocalUsers,
  usersText,
  onChange,
  serverName,
}: {
  allLocalUsers: boolean;
  usersText: string;
  onChange: (patch: { allLocalUsers?: boolean; usersText?: string }) => void;
  serverName?: string;
}) {
  const invalid = parseUserList(usersText).filter((id) => !looksLikeUserId(id));
  const empty = !allLocalUsers && parseUserList(usersText).length === 0;
  return (
    <div>
      <div
        role="radiogroup"
        aria-label="Who can have one"
        className="grid grid-cols-1 gap-3 sm:grid-cols-2"
      >
        <ChoiceCard
          selected={allLocalUsers}
          onSelect={() => onChange({ allLocalUsers: true })}
          title="Everyone on this server"
          description={`Any local user${serverName ? ` (:${serverName})` : ""} can ask for their own.`}
        />
        <ChoiceCard
          selected={!allLocalUsers}
          onSelect={() => onChange({ allLocalUsers: false })}
          title="Only the people I list"
          description="Anyone else who asks is told, once and politely, that it isn't for them."
        />
      </div>
      {!allLocalUsers && (
        <div className="mt-4 max-w-lg">
          <Field
            label="People who can have one"
            hint="Matrix IDs, one per line or separated by commas."
            error={
              invalid.length > 0
                ? `Not a Matrix ID: ${invalid.join(", ")}`
                : empty
                  ? "List at least one person, or let everyone have one."
                  : undefined
            }
          >
            {(f) => (
              <Textarea
                {...f}
                value={usersText}
                placeholder={`@alice:${serverName ?? "example.org"}`}
                onChange={(e) => onChange({ usersText: e.target.value })}
                className="font-identifier"
              />
            )}
          </Field>
        </div>
      )}
    </div>
  );
}

/**
 * Where each instance runs. `cluster` only when this server has a deployment target and the
 * type can run from its config alone; when it can't, the card says why and the explanation
 * below it quotes the server's own reason.
 */
export function RuntimeFields({
  runtime,
  onRuntime,
  clusterAvailable,
  unavailableBecause,
  target,
  type,
  imageTag,
  onImageTag,
}: {
  runtime: BridgeOfferingRuntime;
  onRuntime: (runtime: BridgeOfferingRuntime) => void;
  clusterAvailable: boolean;
  unavailableBecause: "no-target" | "not-deployable" | "loading" | null;
  target: BridgeDeploymentTarget | undefined;
  type: Pick<BridgeType, "name" | "id" | "image"> | undefined;
  imageTag: string;
  onImageTag: (tag: string) => void;
}) {
  const reasonText =
    unavailableBecause === "no-target"
      ? "This server can't deploy bridges."
      : unavailableBecause === "not-deployable"
        ? notDeployableReason(type)
        : unavailableBecause === "loading"
          ? "Checking whether this server can deploy bridges…"
          : null;
  const repository = (type?.image ?? "").split(":")[0];
  return (
    <div>
      <div role="radiogroup" aria-label="Runtime" className="grid grid-cols-1 gap-3 sm:grid-cols-2">
        <ChoiceCard
          selected={runtime === "cluster" && clusterAvailable}
          onSelect={() => onRuntime("cluster")}
          disabled={!clusterAvailable}
          icon={<Cloud size={18} aria-hidden="true" />}
          title="Runs in this cluster"
          description={
            target?.available && target.namespace
              ? `This server deploys each person's bridge as its own pod in ${target.namespace}, with its own volume, and removes it when they're done.`
              : "This server deploys each person's bridge as its own pod, with its own volume, and removes it when they're done."
          }
          note={!clusterAvailable ? reasonText : undefined}
        />
        <ChoiceCard
          selected={runtime === "elsewhere" || !clusterAvailable}
          onSelect={() => onRuntime("elsewhere")}
          icon={<Laptop size={18} aria-hidden="true" />}
          title="Runs elsewhere"
          description="Someone runs each bridge from its files on a machine that can, like the Mac an iMessage bridge needs. You download the files from the offering's page."
        />
      </div>

      {unavailableBecause === "no-target" && (
        <div
          role="note"
          className="mt-4 rounded-md border border-info-border bg-info-bg px-4 py-3 text-sm text-text"
        >
          <p className="font-medium">Why this server can&apos;t run bridges itself</p>
          <p className="mt-1 text-text-muted">
            {target?.reason ?? "It isn't running in Kubernetes with the chart's bridges enabled."}
          </p>
          <p className="mt-1 text-text-muted">
            In Kubernetes, install with the chart&apos;s{" "}
            <code className="font-identifier">bridges.enabled</code> (its default) and this server
            deploys each bridge itself.
          </p>
        </div>
      )}

      <div className="mt-6 max-w-sm">
        <Field
          label="Image tag"
          hint={
            repository
              ? `Of ${repository}. Every new bridge uses it; pin a version to upgrade deliberately.`
              : "Every new bridge uses it; pin a version to upgrade deliberately."
          }
        >
          {(f) => (
            <Input
              {...f}
              value={imageTag}
              onChange={(e) => onImageTag(e.target.value)}
              className="font-identifier"
            />
          )}
        </Field>
      </div>
    </div>
  );
}

/** One on/off setting with what it does, labelled and described for assistive technology. */
export function SettingSwitch({
  label,
  description,
  checked,
  onChange,
  disabled,
  note,
}: {
  label: string;
  description: string;
  checked: boolean;
  onChange: (checked: boolean) => void;
  disabled?: boolean;
  note?: string;
}) {
  const labelId = useId();
  const descriptionId = useId();
  return (
    <div className="flex items-start justify-between gap-4 rounded-md border border-border bg-surface p-4">
      <div>
        <p id={labelId} className="text-sm font-medium text-text">
          {label}
        </p>
        <p id={descriptionId} className="mt-0.5 text-sm text-text-muted">
          {description}
          {note && <span className="mt-1 block text-xs">{note}</span>}
        </p>
      </div>
      <Switch
        checked={checked}
        onCheckedChange={onChange}
        disabled={disabled}
        aria-labelledby={labelId}
        aria-describedby={descriptionId}
      />
    </div>
  );
}

export interface OptionValues {
  encryption: boolean;
  doublePuppeting: boolean;
  backfill: boolean;
}

/** Encryption, double puppeting and backfill, each saying what it does to each person's bridge. */
export function OptionFields({
  values,
  onChange,
  type,
}: {
  values: OptionValues;
  onChange: (patch: Partial<OptionValues>) => void;
  type: Pick<BridgeType, "supports_double_puppeting" | "name"> | undefined;
}) {
  const canPuppet = type?.supports_double_puppeting !== false;
  return (
    <div className="flex flex-col gap-3">
      <SettingSwitch
        label="Encryption"
        description="Encrypted chats stay encrypted through the bridge: each bridge gets its own device, and its registration asks for the device lists and to-device messages that needs."
        checked={values.encryption}
        onChange={(v) => onChange({ encryption: v })}
      />
      <SettingSwitch
        label="Double puppeting"
        description="What someone sends from any Matrix client appears on the other network as them, and what they send there appears here as them. Each bridge may do this for its own person only."
        checked={canPuppet && values.doublePuppeting}
        disabled={!canPuppet}
        note={!canPuppet ? `${type?.name ?? "This bridge"} doesn't support it.` : undefined}
        onChange={(v) => onChange({ doublePuppeting: v })}
      />
      <SettingSwitch
        label="Backfill"
        description="When someone signs in, their recent history on the other network is brought into their rooms, not only what arrives from then on."
        checked={values.backfill}
        onChange={(v) => onChange({ backfill: v })}
      />
    </div>
  );
}
