/**
 * One setting: what it is, where its value came from, and how to change it.
 *
 * Provenance is the point. Every competing homeserver's answer to "why is
 * this set to that?" is to go and read a YAML file on a host; here the row
 * says it outright — the schema default, a bootstrap file, the database, or
 * an environment variable the deployment pinned. The last of those the API
 * will refuse to write, so the row shows it read-only with the reason rather
 * than offering an edit that is going to fail.
 */
import { useId } from "react";
import { RotateCcw, Undo2 } from "lucide-react";
import type { ConfigOrigin, JsonValue } from "@/api/config-schema";
import { formatValue, isChanged, settingRowId, type SettingField } from "@/lib/config-model";
import { Badge } from "@/components/ui/badge/Badge";
import { Button } from "@/components/ui/button/Button";
import { SettingControl } from "./SettingControls";
import { cn } from "@/lib/cn";

const ORIGIN_COPY: Record<ConfigOrigin, { label: string; detail: string }> = {
  default: { label: "Default", detail: "Nothing sets this; it is the schema's own default." },
  file: {
    label: "From file",
    detail: "Set in the bootstrap file. Saving here stores it in the database, which wins.",
  },
  database: {
    label: "From database",
    detail: "Set here, in this server's own configuration store.",
  },
  environment: {
    label: "Pinned by environment",
    detail:
      "An HS__ environment variable sets this — a Kubernetes manifest, a systemd unit, a compose file. It outranks the database, so the API refuses to change it. Change it where the environment is set.",
  },
};

export function OriginBadge({ origin }: { origin: ConfigOrigin }) {
  const copy = ORIGIN_COPY[origin];
  const status = origin === "environment" ? "warning" : origin === "default" ? "neutral" : "info";
  return (
    <Badge status={status} hideIcon={origin !== "environment"}>
      {copy.label}
    </Badge>
  );
}

/** The kinds that render as one labelable control; the rest label their own parts. */
const LABELABLE: ReadonlySet<string> = new Set([
  "boolean",
  "enum",
  "integer",
  "number",
  "string",
  "duration",
  "bytes",
  "secret",
]);

export interface SettingRowProps {
  field: SettingField;
  /** What the server has now. */
  effective: JsonValue | undefined;
  /** What the operator has typed, if anything. `null` is a pending reset. */
  draftValue?: JsonValue | null;
  /** There is a pending edit for this setting — show what was typed, not the server's value. */
  edited: boolean;
  /** That pending edit actually changes something. Typing a value back to what it already was
   * leaves `edited` true (the box keeps the text) but `dirty` false (nothing to announce). */
  dirty: boolean;
  origin?: ConfigOrigin;
  error?: string;
  /** The whole section cannot be written (bootstrap-only, or no `admin:write`). */
  locked?: boolean;
  lockedReason?: string;
  onChange: (value: JsonValue | null) => void;
  onRevert: () => void;
  onReset: () => void;
}

export function SettingRow({
  field,
  effective,
  draftValue,
  edited,
  dirty,
  origin,
  error,
  locked,
  lockedReason,
  onChange,
  onRevert,
  onReset,
}: SettingRowProps) {
  const controlId = useId();
  const hintId = `${controlId}-hint`;
  const errorId = `${controlId}-error`;
  const pendingReset = edited && draftValue === null;

  // A reset shows the default it will revert to, so the control is not blank
  // while the operator decides.
  const shown = edited ? (pendingReset ? field.defaultValue : draftValue) : effective;

  const pinned = origin === "environment";
  // `field.editable` is the server's own answer to "would config.update take
  // this?", so it outranks anything inferred here.
  const readOnly = Boolean(locked) || pinned || field.readOnly || !field.editable;
  const changed = isChanged(field, effective, origin);
  // Resetting sends `null`, which the server reads as RFC 7396's removal: the
  // setting reverts to the schema default, or — for an `Option<T>` with no
  // default — to not being set at all. Both are worth offering. A *required*
  // field with no default is the one case where removal is simply invalid.
  const resettable = field.hasDefault || !field.required;
  const canReset = !readOnly && resettable && (changed || dirty);
  const resetLabel = field.hasDefault ? "Reset to default" : "Unset";

  const describedBy = [hintId, error ? errorId : undefined].filter(Boolean).join(" ") || undefined;
  const useLabel = LABELABLE.has(field.kind) && !readOnly;

  return (
    <div
      id={settingRowId(field.path)}
      className={cn(
        "grid scroll-mt-24 gap-x-8 gap-y-3 px-4 py-4 sm:grid-cols-[minmax(0,1fr)_minmax(0,22rem)]",
        dirty && "bg-accent-muted/40",
        error && "bg-danger-bg",
      )}
    >
      <div className="min-w-0">
        {useLabel ? (
          <label htmlFor={controlId} className="text-sm font-medium text-text">
            {field.label}
            {field.required && !field.hasDefault && (
              <span aria-hidden="true" className="text-danger">
                {" "}
                *
              </span>
            )}
          </label>
        ) : (
          <p className="text-sm font-medium text-text">{field.label}</p>
        )}
        <p className="font-identifier text-xs text-text-faint">{field.fullPath}</p>

        {field.summary && (
          <p id={hintId} className="mt-1.5 text-sm text-text-muted">
            {field.summary}
          </p>
        )}
        {field.description && (
          <details className="mt-1.5">
            <summary className="cursor-pointer text-xs text-accent hover:underline">
              More about this setting
            </summary>
            <p className="mt-1 text-sm text-text-muted">{field.description}</p>
          </details>
        )}

        <div className="mt-2.5 flex flex-wrap items-center gap-2">
          {origin && <OriginBadge origin={origin} />}
          {changed && !pinned && (
            <Badge status="info" hideIcon>
              Changed from default
            </Badge>
          )}
          {edited && (
            <Badge status="warning">
              {pendingReset
                ? field.hasDefault
                  ? "Will reset to default"
                  : "Will be unset"
                : "Edited"}
            </Badge>
          )}
          {canReset && !pendingReset && (
            <Button
              variant="ghost"
              size="sm"
              leadingIcon={<RotateCcw size={14} aria-hidden="true" />}
              aria-label={
                field.hasDefault ? `Reset ${field.label} to its default` : `Unset ${field.label}`
              }
              onClick={onReset}
            >
              {resetLabel}
            </Button>
          )}
          {edited && (
            <Button
              variant="ghost"
              size="sm"
              leadingIcon={<Undo2 size={14} aria-hidden="true" />}
              aria-label={`Undo the change to ${field.label}`}
              onClick={onRevert}
            >
              Undo
            </Button>
          )}
        </div>

        {pinned && <p className="mt-2 text-xs text-warning">{ORIGIN_COPY.environment.detail}</p>}
        {!pinned && !locked && !field.editable && !field.readOnly && (
          <p className="mt-2 text-xs text-warning">
            This server will not accept a change to this setting.
          </p>
        )}
        {locked && lockedReason && <p className="mt-2 text-xs text-text-muted">{lockedReason}</p>}
      </div>

      <div className="min-w-0">
        {readOnly ? (
          <p className="flex min-h-9 items-center break-words font-identifier text-sm text-text">
            {formatValue(shown)}
          </p>
        ) : (
          <SettingControl
            field={field}
            value={shown}
            id={controlId}
            describedBy={describedBy}
            invalid={Boolean(error)}
            onChange={onChange}
            onRevert={onRevert}
          />
        )}
        {error && (
          <p id={errorId} role="alert" className="mt-1.5 text-xs text-danger">
            {error}
          </p>
        )}
        {field.hasDefault && !readOnly && (
          <p className="mt-1.5 text-xs text-text-faint">
            Default: <span className="font-identifier">{formatValue(field.defaultValue)}</span>
          </p>
        )}
      </div>
    </div>
  );
}
