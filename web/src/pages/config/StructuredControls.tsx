/**
 * The structured editors: what the Configuration page renders for a setting
 * that is more than one value — a list of listeners, a list of OIDC
 * providers, the media storage backend, a map.
 *
 * Decision 0010 is the reason this file exists: no page in this interface
 * edits YAML, JSON or any file format as text. Each editor here is built from
 * the setting's JSON Schema (`lib/config-model.ts` turns every nested property
 * into a {@link SettingField} of its own), and each nested field is rendered
 * with the same controls a top-level setting gets, recursively. So a new
 * field on `hs_config::Listener` appears in every listener's form as soon as
 * the server describes it, and nothing here names a setting.
 *
 * The same conventions as `SettingControls.tsx` hold: every control is
 * controlled by the section page's draft and reports the whole new value of
 * its setting (a list is replaced wholesale by an RFC 7396 merge patch, so
 * the whole list is what has to be sent). Inside a nested form, `null` from a
 * child means "leave this property out", which is how an unset `Option<T>`
 * and a property left to its default are both spelled.
 */
import { useEffect, useId, useRef, useState } from "react";
import { ArrowDown, ArrowUp, Plus, Trash2 } from "lucide-react";
import type { JsonValue } from "@/api/config-schema";
import {
  SCALAR_KINDS,
  STRUCTURED_KINDS,
  choiceInfo,
  choicePayloadField,
  chosenChoice,
  chosenVariant,
  switchChoice,
  markSecretOrigins,
  pointerOf,
  emptyValue,
  entryNoun,
  entrySummary,
  formatValue,
  humanizeKey,
  itemField,
  mapEntryField,
  propertyFields,
  switchVariant,
  variantInfo,
  type SettingField,
} from "@/lib/config-model";
import { Button } from "@/components/ui/button/Button";
import { Input } from "@/components/ui/input/Input";
import { Select } from "@/components/ui/select/Select";
import { cn } from "@/lib/cn";
import { SettingControl, type ControlProps } from "./SettingControls";

function isRecord(value: unknown): value is Record<string, JsonValue> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** "Listener" → "listener", but "OIDC provider" stays as it is. */
function sentenceNoun(noun: string): string {
  return /^[A-Z][A-Z]/.test(noun) ? noun : noun.charAt(0).toLowerCase() + noun.slice(1);
}

// ---------------------------------------------------------------------------
// One nested setting
// ---------------------------------------------------------------------------

interface NestedFieldProps {
  field: SettingField;
  value: JsonValue | undefined;
  disabled?: boolean;
  onChange: (value: JsonValue | null) => void;
}

/**
 * One property inside a nested form: its label, its hint, and the control
 * its kind gets — the very same control a top-level setting of that kind
 * gets, including another nested form.
 */
function NestedField({ field, value, disabled, onChange }: NestedFieldProps) {
  const id = useId();
  const labelId = `${id}-label`;
  const hintId = `${id}-hint`;
  // A secret's "Cancel" puts back what was there before "Replace" was
  // pressed: the redaction marker, which the server reads as "unchanged".
  const [before] = useState(value);
  const labelable = SCALAR_KINDS.has(field.kind) && !field.readOnly;
  const structured = STRUCTURED_KINDS.has(field.kind);
  const required = field.required && !field.hasDefault;

  const label = (
    <>
      {field.label}
      {required && (
        <span aria-hidden="true" className="text-danger">
          {" "}
          *
        </span>
      )}
    </>
  );

  return (
    <div className={cn("min-w-0", structured && "sm:col-span-2")}>
      {labelable ? (
        <label id={labelId} htmlFor={id} className="text-sm font-medium text-text">
          {label}
        </label>
      ) : (
        <p id={labelId} className="text-sm font-medium text-text">
          {label}
        </p>
      )}
      {field.summary && (
        <p id={hintId} className="mb-1.5 text-xs text-text-muted">
          {field.summary}
        </p>
      )}
      <div className={field.summary ? undefined : "mt-1.5"}>
        {field.readOnly ? (
          <p className="font-identifier text-sm text-text">{formatValue(value)}</p>
        ) : (
          <SettingControl
            field={field}
            value={value}
            id={id}
            describedBy={field.summary ? hintId : undefined}
            labelledBy={labelId}
            disabled={disabled}
            onChange={onChange}
            onRevert={() => onChange(before === undefined ? null : before)}
          />
        )}
      </div>
    </div>
  );
}

/** The properties of one object, two to a row where there is room. */
function PropertyGrid({
  fields,
  value,
  disabled,
  onChange,
}: {
  fields: SettingField[];
  value: Record<string, JsonValue>;
  disabled?: boolean;
  onChange: (next: Record<string, JsonValue>) => void;
}) {
  if (fields.length === 0) {
    return <p className="text-sm text-text-muted">Nothing else to set for this one.</p>;
  }
  return (
    <div className="grid gap-x-6 gap-y-4 sm:grid-cols-2">
      {fields.map((sub) => (
        <NestedField
          key={sub.key}
          field={sub}
          value={value[sub.key]}
          disabled={disabled}
          onChange={(next) => {
            const updated = { ...value };
            if (next === null) delete updated[sub.key];
            else updated[sub.key] = next;
            onChange(updated);
          }}
        />
      ))}
    </div>
  );
}

// ---------------------------------------------------------------------------
// The editors
// ---------------------------------------------------------------------------

/**
 * A fixed set of named settings, as one nested form. An `Option<T>` of an
 * object (a listener's `tls`) can be left unset, so it offers to set one up
 * and, once set, to remove it again.
 */
export function ObjectControl({
  field,
  value,
  disabled,
  id,
  describedBy,
  labelledBy,
  onChange,
}: ControlProps) {
  const record = isRecord(value) ? value : undefined;
  const noun = sentenceNoun(field.label);

  if (!record && field.nullable) {
    return (
      <div
        id={id}
        role="group"
        aria-labelledby={labelledBy}
        aria-describedby={describedBy}
        className="flex flex-wrap items-center gap-3"
      >
        <p className="text-sm text-text-muted">Not set.</p>
        <Button
          variant="secondary"
          size="sm"
          disabled={disabled}
          leadingIcon={<Plus size={14} aria-hidden="true" />}
          onClick={() => onChange(emptyValue(field))}
        >
          Set up {noun}
        </Button>
      </div>
    );
  }

  return (
    <div
      id={id}
      role="group"
      aria-labelledby={labelledBy}
      aria-describedby={describedBy}
      className="flex flex-col gap-3 rounded-md border border-border p-3"
    >
      <PropertyGrid
        fields={propertyFields(field, record)}
        value={record ?? {}}
        disabled={disabled}
        onChange={onChange}
      />
      {field.nullable && (
        <div>
          <Button
            variant="ghost"
            size="sm"
            disabled={disabled}
            leadingIcon={<Trash2 size={14} aria-hidden="true" />}
            onClick={() => onChange(null)}
          >
            Remove {noun}
          </Button>
        </div>
      )}
    </div>
  );
}

/**
 * An internally tagged enum — `media.storage` is `local`, `s3`, `gcs` or
 * `azure`, each with settings of its own. The tag is a select; the chosen
 * variant's settings are a nested form. Switching keeps any setting the two
 * variants share and starts the rest from their defaults.
 */
export function VariantControl(props: ControlProps) {
  const { field, value, disabled, id, describedBy, labelledBy, onChange } = props;
  const pickerId = useId();
  const pickerHintId = `${pickerId}-hint`;
  const info = variantInfo(field.schema, field.defs ?? {});
  if (!info) return <UnsupportedControl {...props} />;

  const chosen = chosenVariant(field, value);
  const record = isRecord(value) ? value : {};

  return (
    <div
      id={id}
      role="group"
      aria-labelledby={labelledBy}
      aria-describedby={describedBy}
      className="flex flex-col gap-4 rounded-md border border-border p-3"
    >
      <div className="max-w-xs">
        <label htmlFor={pickerId} className="text-sm font-medium text-text">
          {humanizeKey(info.tag)}
        </label>
        <Select
          id={pickerId}
          options={info.options.map(({ value: optionValue, label }) => ({
            value: optionValue,
            label,
          }))}
          value={chosen?.value}
          placeholder="Choose one"
          disabled={disabled}
          aria-describedby={chosen?.description ? pickerHintId : undefined}
          onValueChange={(tag) => onChange(switchVariant(field, value, tag))}
        />
        {chosen?.description && (
          <p id={pickerHintId} className="mt-1 text-xs text-text-muted">
            {chosen.description}
          </p>
        )}
      </div>
      {chosen && (
        <PropertyGrid
          fields={propertyFields(field, record)}
          value={record}
          disabled={disabled}
          onChange={(next) => onChange({ ...next, [info.tag]: chosen.value })}
        />
      )}
    </div>
  );
}

/**
 * An externally tagged enum — `media.scanning.icap.preview` is `negotiate`,
 * `off`, or a forced size (`{"bytes": N}`). The choice is a select; a choice
 * that carries a value gets that value's own control beneath it.
 */
export function ChoiceControl(props: ControlProps) {
  const { field, value, disabled, id, describedBy, labelledBy, onChange } = props;
  const pickerId = useId();
  const pickerHintId = `${pickerId}-hint`;
  const options = choiceInfo(field.schema, field.defs ?? {});
  if (!options) return <UnsupportedControl {...props} />;

  const chosen = chosenChoice(field, value);
  const payload = chosen?.payload ? choicePayloadField(field, chosen, value) : undefined;

  return (
    <div
      id={id}
      role="group"
      aria-labelledby={labelledBy}
      aria-describedby={describedBy}
      className="flex flex-col gap-4 rounded-md border border-border p-3"
    >
      <div className="max-w-xs">
        <label htmlFor={pickerId} className="text-sm font-medium text-text">
          {field.label}
        </label>
        <Select
          id={pickerId}
          options={options.map(({ value: optionValue, label }) => ({ value: optionValue, label }))}
          value={chosen?.value}
          placeholder="Choose one"
          disabled={disabled}
          aria-describedby={chosen?.description ? pickerHintId : undefined}
          onValueChange={(choice) => onChange(switchChoice(field, value, choice))}
        />
        {chosen?.description && (
          <p id={pickerHintId} className="mt-1 text-xs text-text-muted">
            {chosen.description}
          </p>
        )}
      </div>
      {chosen && payload && (
        <div className="max-w-xs">
          <NestedField
            field={payload}
            value={isRecord(value) ? value[chosen.value] : undefined}
            disabled={disabled}
            onChange={(next) => onChange({ [chosen.value]: next ?? emptyValue(payload) })}
          />
        </div>
      )}
    </div>
  );
}

/**
 * A list of objects: one form per entry, each with its own controls to move
 * it up or down and to remove it, and one button to add another. Keyboard
 * focus follows the entry being moved, lands in a new entry's first field,
 * and never falls back to the top of the page when an entry is removed.
 */
export function ObjectListControl({
  field,
  value,
  disabled,
  id,
  describedBy,
  labelledBy,
  onChange,
}: ControlProps) {
  const entries = Array.isArray(value) ? value : [];
  const noun = entryNoun(field);
  const baseId = useId();
  const addId = `${baseId}-add`;
  // Where focus goes after the list re-renders: an element id, and whether to
  // focus the element itself or the first field inside it.
  const pendingFocus = useRef<{ id: string; inside: boolean } | null>(null);

  useEffect(() => {
    const target = pendingFocus.current;
    if (!target) return;
    pendingFocus.current = null;
    const element = document.getElementById(target.id);
    const focusable = target.inside
      ? (element?.querySelector<HTMLElement>(
          "input:not([type=hidden]):not([disabled]), [role=combobox]:not([disabled]), [role=switch]:not([disabled])",
        ) ?? element?.querySelector<HTMLElement>("button:not([disabled])"))
      : element;
    focusable?.focus();
  });

  const entryId = (index: number) => `${baseId}-entry-${index}`;
  const controlId = (index: number, action: string) => `${baseId}-entry-${index}-${action}`;

  // Before entries shift, every hidden secret says where it is stored now, so
  // the server can put the right one back (docs/rfcs/0020).
  const marked = () =>
    entries.map((entry, index) =>
      markSecretOrigins(entry, `${pointerOf(field.fullPath)}/${index}`),
    );

  function move(from: number, to: number) {
    const next = marked();
    const [moved] = next.splice(from, 1);
    next.splice(to, 0, moved);
    // Stay on the same button, unless the entry has reached the end it was
    // moving towards, where that button is now disabled.
    const direction = to < from ? "up" : "down";
    const atEdge = to === 0 || to === entries.length - 1;
    const action = atEdge ? (direction === "up" ? "down" : "up") : direction;
    pendingFocus.current = { id: controlId(to, action), inside: false };
    onChange(next);
  }

  function remove(index: number) {
    const next = marked().filter((_, i) => i !== index);
    pendingFocus.current =
      next.length === 0
        ? { id: addId, inside: false }
        : { id: controlId(Math.min(index, next.length - 1), "remove"), inside: false };
    onChange(next);
  }

  function add() {
    const blank = emptyValue(itemField(field, entries.length, undefined));
    pendingFocus.current = { id: `${entryId(entries.length)}-body`, inside: true };
    onChange([...entries, blank]);
  }

  return (
    <div
      id={id}
      role="group"
      aria-labelledby={labelledBy}
      aria-describedby={describedBy}
      className="flex flex-col gap-3"
    >
      {entries.length === 0 && <p className="text-sm text-text-muted">Empty list.</p>}
      {entries.length > 0 && (
        <ol className="flex flex-col gap-3">
          {entries.map((entry, index) => {
            const sub = itemField(field, index, entry);
            const summary = entrySummary(sub, entry);
            const titleId = `${entryId(index)}-title`;
            return (
              // Index keys on purpose: every control in an entry is
              // controlled by the draft, so there is no per-entry state for
              // a reorder to strand.
              <li key={index} id={entryId(index)}>
                <div
                  role="group"
                  aria-labelledby={titleId}
                  className="rounded-md border border-border bg-surface p-3"
                >
                  <div className="flex flex-wrap items-center justify-between gap-2">
                    <p id={titleId} className="min-w-0 text-sm font-medium text-text">
                      {sub.label}
                      {summary && (
                        <span className="font-identifier font-normal text-text-muted">
                          {" "}
                          · {summary}
                        </span>
                      )}
                    </p>
                    <div className="flex gap-1">
                      <Button
                        id={controlId(index, "up")}
                        variant="ghost"
                        size="icon"
                        disabled={disabled || index === 0}
                        aria-label={`Move ${sub.label} up`}
                        onClick={() => move(index, index - 1)}
                      >
                        <ArrowUp size={16} aria-hidden="true" />
                      </Button>
                      <Button
                        id={controlId(index, "down")}
                        variant="ghost"
                        size="icon"
                        disabled={disabled || index === entries.length - 1}
                        aria-label={`Move ${sub.label} down`}
                        onClick={() => move(index, index + 1)}
                      >
                        <ArrowDown size={16} aria-hidden="true" />
                      </Button>
                      <Button
                        id={controlId(index, "remove")}
                        variant="ghost"
                        size="icon"
                        disabled={disabled}
                        aria-label={`Remove ${sub.label}`}
                        onClick={() => remove(index)}
                      >
                        <Trash2 size={16} aria-hidden="true" />
                      </Button>
                    </div>
                  </div>
                  <div id={`${entryId(index)}-body`} className="mt-3">
                    <EntryBody
                      field={sub}
                      value={entry}
                      disabled={disabled}
                      labelledBy={titleId}
                      onChange={(next) => {
                        if (next === null) {
                          remove(index);
                          return;
                        }
                        const updated = [...entries];
                        updated[index] = next;
                        onChange(updated);
                      }}
                    />
                  </div>
                </div>
              </li>
            );
          })}
        </ol>
      )}
      <div>
        <Button
          id={addId}
          variant="secondary"
          size="sm"
          disabled={disabled}
          leadingIcon={<Plus size={14} aria-hidden="true" />}
          onClick={add}
        >
          Add {sentenceNoun(noun)}
        </Button>
      </div>
    </div>
  );
}

/** The inside of one list entry: its nested form, without a second border around it. */
function EntryBody({
  field,
  value,
  disabled,
  labelledBy,
  onChange,
}: {
  field: SettingField;
  value: JsonValue;
  disabled?: boolean;
  labelledBy: string;
  onChange: (value: JsonValue | null) => void;
}) {
  const id = useId();
  if (field.kind === "object") {
    return (
      <PropertyGrid
        fields={propertyFields(field, value)}
        value={isRecord(value) ? value : {}}
        disabled={disabled}
        onChange={onChange}
      />
    );
  }
  return (
    <SettingControl
      field={field}
      value={value}
      id={id}
      labelledBy={labelledBy}
      disabled={disabled}
      onChange={onChange}
      onRevert={() => undefined}
    />
  );
}

/**
 * A map from names to values of one kind: one row per key, each with the
 * control its value's kind gets. A key is typed once, when the entry is
 * added; renaming one is removing it and adding it again, which keeps two
 * entries from ever colliding mid-edit.
 */
export function MapControl({
  field,
  value,
  disabled,
  id,
  describedBy,
  labelledBy,
  onChange,
}: ControlProps) {
  const record = isRecord(value) ? value : {};
  const keys = Object.keys(record);
  const newKeyId = useId();
  const newKeyErrorId = `${newKeyId}-error`;
  const [newKey, setNewKey] = useState("");
  const trimmed = newKey.trim();
  const duplicate = trimmed !== "" && trimmed in record;

  function add() {
    if (trimmed === "" || duplicate) return;
    onChange({ ...record, [trimmed]: emptyValue(mapEntryField(field, trimmed, undefined)) });
    setNewKey("");
  }

  return (
    <div
      id={id}
      role="group"
      aria-labelledby={labelledBy}
      aria-describedby={describedBy}
      className="flex flex-col gap-3"
    >
      {keys.length === 0 && <p className="text-sm text-text-muted">No entries.</p>}
      {keys.length > 0 && (
        <ul className="flex flex-col gap-3">
          {keys.map((key) => (
            <li
              key={key}
              className="flex items-start gap-2 rounded-md border border-border bg-surface p-3"
            >
              <div className="min-w-0 flex-1">
                <NestedField
                  field={mapEntryField(field, key, record[key])}
                  value={record[key]}
                  disabled={disabled}
                  onChange={(next) => {
                    // Emptying a value's box empties the value; only the
                    // Remove button removes the key.
                    const entry = mapEntryField(field, key, record[key]);
                    onChange({ ...record, [key]: next === null ? emptyValue(entry) : next });
                  }}
                />
              </div>
              <Button
                variant="ghost"
                size="icon"
                disabled={disabled}
                aria-label={`Remove ${key}`}
                onClick={() => {
                  const updated = { ...record };
                  delete updated[key];
                  onChange(updated);
                }}
              >
                <Trash2 size={16} aria-hidden="true" />
              </Button>
            </li>
          ))}
        </ul>
      )}
      <div className="flex flex-wrap items-start gap-2">
        <div className="min-w-0 max-w-xs flex-1">
          <label htmlFor={newKeyId} className="sr-only">
            New key for {field.label}
          </label>
          <Input
            id={newKeyId}
            type="text"
            value={newKey}
            disabled={disabled}
            placeholder="New key"
            aria-invalid={duplicate}
            aria-describedby={duplicate ? newKeyErrorId : undefined}
            className="font-identifier"
            onChange={(e) => setNewKey(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") {
                e.preventDefault();
                add();
              }
            }}
          />
          {duplicate && (
            <p id={newKeyErrorId} role="alert" className="mt-1 text-xs text-danger">
              {trimmed} is already in the list.
            </p>
          )}
        </div>
        <Button
          variant="secondary"
          size="sm"
          className="mt-0.5"
          disabled={disabled || trimmed === "" || duplicate}
          leadingIcon={<Plus size={14} aria-hidden="true" />}
          onClick={add}
        >
          Add entry
        </Button>
      </div>
    </div>
  );
}

/**
 * A shape the schema describes in a way this interface does not recognise.
 * Shown read-only, with a note saying so, rather than as text to edit
 * (decision 0010). Every setting the real server has today gets a real
 * control — `config-model.real-schema.test.ts` checks exactly that — so this
 * is what a server newer than this build looks like, not a normal state.
 */
export function UnsupportedControl({ field, value, id, describedBy, labelledBy }: ControlProps) {
  return (
    <div
      id={id}
      role="group"
      aria-labelledby={labelledBy}
      aria-describedby={describedBy}
      className="flex flex-col gap-2"
    >
      <ReadOnlyValue value={value} />
      <p role="note" className="text-xs text-text-muted">
        This interface cannot edit {field.label.toLowerCase()} yet: its shape is not one it
        recognises. It is shown read-only rather than as text to edit; change it through the admin
        API (<span className="font-identifier">PATCH /config/{field.fullPath.split(".")[0]}</span>
        ).
      </p>
    </div>
  );
}

/**
 * Any value, read-only, as nested lists and name/value pairs rather than as
 * JSON — what a pinned, locked or bootstrap-only structured setting shows.
 */
export function ReadOnlyValue({ value }: { value: JsonValue | undefined }) {
  if (Array.isArray(value) && value.some((v) => typeof v === "object" && v !== null)) {
    return (
      <ol className="flex list-decimal flex-col gap-2 pl-5 text-sm text-text">
        {value.map((entry, index) => (
          <li key={index}>
            <ReadOnlyValue value={entry} />
          </li>
        ))}
      </ol>
    );
  }
  if (isRecord(value) && !("$secret" in value)) {
    const entries = Object.entries(value);
    if (entries.length === 0) return <p className="text-sm text-text-muted">Empty.</p>;
    return (
      <dl className="grid grid-cols-[auto_minmax(0,1fr)] gap-x-3 gap-y-1 text-sm">
        {entries.map(([key, v]) => (
          <div key={key} className="contents">
            <dt className="text-text-muted">{humanizeKey(key)}</dt>
            <dd className="min-w-0 break-words text-text">
              <ReadOnlyValue value={v} />
            </dd>
          </div>
        ))}
      </dl>
    );
  }
  return <span className="font-identifier text-sm text-text">{formatValue(value)}</span>;
}
