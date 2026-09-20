/**
 * One control per {@link FieldKind}. Everything here is controlled by the
 * section page's draft: a control never holds the authoritative value, it
 * only reports what the operator did to it. There is deliberately no local
 * copy of the value in any of these — no `useState`, no effect syncing one to
 * the other, and so no way for what is on screen to drift from what will be
 * saved.
 *
 * Three conventions the whole set shares:
 *
 * - **Emitting `null` means "reset to default".** That is RFC 7396's removal
 *   and the server's reset (`crates/hs-config/src/document.rs`), so clearing
 *   a box is the same gesture as pressing "Reset to default" — not "set this
 *   to null".
 * - **Half-typed input stays a string.** A duration mid-edit (`1h3`), a
 *   number mid-edit (`0.`) and a JSON object mid-edit are all held in the
 *   draft as the raw text and only converted on blur. The draft can briefly
 *   hold a string where a number belongs, which is harmless: nothing reads it
 *   until the operator saves, by which point blur has happened.
 * - **A secret is never read back.** The API answers `{"$secret": true}` and
 *   has no endpoint that reveals one, so the control offers "replace" and
 *   "clear", never "show".
 */
import { useId } from "react";
import { Eye, Plus, X } from "lucide-react";
import type { JsonValue } from "@/api/config-schema";
import type { SettingField } from "@/lib/config-model";
import { Button } from "@/components/ui/button/Button";
import { Input, Textarea } from "@/components/ui/input/Input";
import { Select } from "@/components/ui/select/Select";
import { Switch } from "@/components/ui/switch/Switch";
import { cn } from "@/lib/cn";

export interface ControlProps {
  field: SettingField;
  /** What the form currently shows: the effective value, or the operator's edit. */
  value: JsonValue | undefined;
  disabled?: boolean;
  invalid?: boolean;
  id: string;
  describedBy?: string;
  onChange: (value: JsonValue | null) => void;
  /** Drops this setting's pending edit, leaving the server's value alone. */
  onRevert: () => void;
}

/** Picks the control for a field's kind. */
export function SettingControl(props: ControlProps) {
  switch (props.field.kind) {
    case "boolean":
      return <BooleanControl {...props} />;
    case "enum":
      return <EnumControl {...props} />;
    case "integer":
    case "number":
      return <NumberControl {...props} />;
    case "secret":
      return <SecretControl {...props} />;
    case "string-list":
    case "number-list":
      return <ListControl {...props} />;
    case "json":
      return <JsonControl {...props} />;
    default:
      return <TextControl {...props} />;
  }
}

/** Whatever the value is, as the text an input should show. */
function asText(value: JsonValue | undefined): string {
  if (value === undefined || value === null) return "";
  if (typeof value === "string") return value;
  if (typeof value === "number" || typeof value === "boolean") return String(value);
  return JSON.stringify(value, null, 2);
}

function BooleanControl({ value, disabled, id, describedBy, onChange }: ControlProps) {
  return (
    <div className="flex h-9 items-center">
      <Switch
        id={id}
        checked={value === true}
        disabled={disabled}
        aria-describedby={describedBy}
        onCheckedChange={(next) => onChange(next)}
      />
    </div>
  );
}

function EnumControl({ field, value, disabled, invalid, id, describedBy, onChange }: ControlProps) {
  return (
    <Select
      id={id}
      options={field.options ?? []}
      value={typeof value === "string" ? value : undefined}
      placeholder="Not set"
      disabled={disabled}
      aria-describedby={describedBy}
      aria-invalid={invalid}
      onValueChange={(next) => onChange(next)}
    />
  );
}

/** Text, duration and byte-size fields. Emptying the box is a reset. */
function TextControl({ field, value, disabled, invalid, id, describedBy, onChange }: ControlProps) {
  return (
    <Input
      id={id}
      type="text"
      value={asText(value)}
      disabled={disabled}
      readOnly={field.readOnly}
      placeholder={field.unitHint ?? "Not set"}
      aria-describedby={describedBy}
      aria-invalid={invalid}
      className="font-identifier"
      onChange={(e) => onChange(e.target.value)}
      onBlur={(e) => {
        if (e.target.value === "") onChange(null);
      }}
    />
  );
}

/**
 * Numbers. Deliberately `type="text"` with a numeric `inputMode`: a real
 * `type="number"` input discards any value that is not a valid floating-point
 * number the moment it is set, so a controlled one cannot hold `0.` long
 * enough for the operator to type the digit after the point. The schema's
 * range is shown by the row as a hint rather than enforced by the widget; the
 * server validates it either way.
 */
function NumberControl({
  field,
  value,
  disabled,
  invalid,
  id,
  describedBy,
  onChange,
}: ControlProps) {
  return (
    <Input
      id={id}
      type="text"
      inputMode={field.kind === "integer" ? "numeric" : "decimal"}
      value={asText(value)}
      disabled={disabled}
      placeholder="Not set"
      aria-describedby={describedBy}
      aria-invalid={invalid}
      className="font-identifier"
      onChange={(e) => onChange(e.target.value)}
      onBlur={(e) => {
        const raw = e.target.value.trim();
        if (raw === "") {
          onChange(null);
          return;
        }
        const parsed = Number(raw);
        // An unparsable number is sent as typed rather than swallowed: the
        // server answers with the real validation error, which is more use
        // than a control that silently refuses input.
        onChange(Number.isFinite(parsed) ? parsed : raw);
      }}
    />
  );
}

/**
 * A secret. `{"$secret": true}` means one is stored; anything else means none
 * is. Replacing reveals an empty password box, never the stored value.
 */
function SecretControl({
  field,
  value,
  disabled,
  invalid,
  id,
  describedBy,
  onChange,
  onRevert,
}: ControlProps) {
  const isSet = typeof value === "object" && value !== null && !Array.isArray(value);

  if (typeof value === "string") {
    return (
      <div className="flex items-center gap-2">
        <Input
          id={id}
          type="password"
          autoComplete="new-password"
          value={value}
          disabled={disabled}
          placeholder="New value"
          aria-describedby={describedBy}
          aria-invalid={invalid}
          aria-label={`New value for ${field.label}`}
          onChange={(e) => onChange(e.target.value)}
        />
        <Button variant="ghost" size="sm" onClick={onRevert}>
          Cancel
        </Button>
      </div>
    );
  }

  return (
    <div className="flex h-9 items-center gap-2">
      <span
        className={cn(
          "inline-flex items-center gap-1.5 text-sm",
          isSet ? "text-text" : "text-text-muted",
        )}
      >
        <Eye size={14} aria-hidden="true" className="text-text-faint" />
        {isSet ? "Set, hidden" : "Not set"}
      </span>
      <Button
        id={id}
        variant="secondary"
        size="sm"
        disabled={disabled}
        aria-describedby={describedBy}
        aria-label={`${isSet ? "Replace" : "Set"} ${field.label}`}
        onClick={() => onChange("")}
      >
        {isSet ? "Replace" : "Set"}
      </Button>
    </div>
  );
}

/** An array of scalars, edited one entry at a time. */
function ListControl({ field, value, disabled, invalid, id, describedBy, onChange }: ControlProps) {
  const numeric = field.kind === "number-list";
  const entries = Array.isArray(value) ? value : [];
  const listId = useId();

  return (
    <div className="flex flex-col gap-2" aria-describedby={describedBy}>
      {entries.length === 0 && <p className="text-sm text-text-muted">Empty list.</p>}
      <ul className="flex flex-col gap-2">
        {entries.map((entry, index) => (
          <li key={`${listId}-${index}`} className="flex items-center gap-2">
            <Input
              id={index === 0 ? id : undefined}
              type="text"
              inputMode={numeric ? "decimal" : undefined}
              value={asText(entry)}
              disabled={disabled}
              aria-invalid={invalid}
              aria-label={`${field.label}, entry ${index + 1}`}
              className="font-identifier"
              onChange={(e) => {
                const raw = e.target.value;
                const next = [...entries];
                next[index] =
                  numeric && raw !== "" && Number.isFinite(Number(raw)) ? Number(raw) : raw;
                onChange(next);
              }}
            />
            <Button
              variant="ghost"
              size="icon"
              disabled={disabled}
              aria-label={`Remove entry ${index + 1} from ${field.label}`}
              onClick={() => onChange(entries.filter((_, i) => i !== index))}
            >
              <X size={16} aria-hidden="true" />
            </Button>
          </li>
        ))}
      </ul>
      <div>
        <Button
          id={entries.length === 0 ? id : undefined}
          variant="secondary"
          size="sm"
          disabled={disabled}
          aria-label={`Add an entry to ${field.label}`}
          leadingIcon={<Plus size={14} aria-hidden="true" />}
          onClick={() => onChange([...entries, numeric ? 0 : ""])}
        >
          Add entry
        </Button>
      </div>
    </div>
  );
}

/**
 * Anything with no better control: an array of objects, a map, a variant this
 * build does not recognise. Edited as JSON rather than hidden, because an
 * operator who cannot reach a setting at all is worse off than one who has to
 * type `{"width": 96}`. Text that does not parse stays in the box, as text,
 * with the parser's complaint under it.
 */
function JsonControl({ field, value, disabled, invalid, id, describedBy, onChange }: ControlProps) {
  const text = asText(value);
  const parseError = typeof value === "string" && value.trim() !== "" ? jsonError(value) : null;
  const errorId = `${id}-json-error`;

  return (
    <div className="flex flex-col gap-1.5">
      <Textarea
        id={id}
        value={text}
        rows={Math.min(14, Math.max(3, text.split("\n").length))}
        disabled={disabled}
        readOnly={field.readOnly}
        spellCheck={false}
        aria-describedby={[describedBy, parseError ? errorId : undefined].filter(Boolean).join(" ")}
        aria-invalid={invalid || Boolean(parseError)}
        aria-label={`${field.label}, as JSON`}
        className="font-mono text-sm"
        onChange={(e) => onChange(e.target.value)}
        onBlur={(e) => {
          const raw = e.target.value;
          if (raw.trim() === "") {
            onChange(null);
            return;
          }
          try {
            onChange(JSON.parse(raw) as JsonValue);
          } catch {
            // Leave it as text: `parseError` above is what the operator sees.
          }
        }}
      />
      {parseError && (
        <p id={errorId} role="alert" className="text-xs text-danger">
          Not valid JSON: {parseError}
        </p>
      )}
    </div>
  );
}

function jsonError(raw: string): string | null {
  try {
    JSON.parse(raw);
    return null;
  } catch (err) {
    return err instanceof Error ? err.message : "Not valid JSON";
  }
}
