/**
 * Turning the configuration JSON Schema into something renderable, and back
 * into an RFC 7396 merge patch.
 *
 * The configuration has ten sections and hundreds of settings, all of them
 * generated from Rust structs (`crates/hs-config/src/*.rs`, `docs/config.md`).
 * A hardcoded form would rot the moment one of those structs gains a field,
 * so the form is built here from `GET /config/schema` instead: this module
 * walks the schema into {@link SettingGroup}s of {@link SettingField}s, and
 * `pages/config/` renders one control per field kind.
 *
 * Everything here is pure — no React, no fetch — because the parts worth
 * being sure about (what counts as changed from default, where a validation
 * error lands, what patch a set of edits produces) are exactly the parts a
 * test can pin down without a browser.
 */
import {
  resolveRef,
  type ConfigOrigin,
  type ConfigSchemaModel,
  type ConfigSettingInfo,
  type JsonSchemaNode,
  type JsonValue,
} from "@/api/config-schema";

/**
 * How one setting is edited. Chosen from the schema, with one exception:
 * `secret` can also be forced by the *value* (`{"$secret": true}`), since the
 * API redacts secrets whether or not the schema says which fields they are.
 *
 * There is deliberately no "edit it as JSON" kind (decision 0010: no page
 * edits a file format as text). Every shape the configuration schema uses has
 * a real control; a shape this build does not recognise is `unsupported`,
 * rendered read-only, and is a bug to fix here rather than a reason for a
 * text box.
 */
export type FieldKind =
  | "boolean"
  | "enum"
  | "integer"
  | "number"
  | "string"
  | "duration"
  | "bytes"
  | "secret"
  | "string-list"
  | "number-list"
  /** An array whose entries come from a fixed set of strings: one checkbox each. */
  | "enum-list"
  /** A fixed set of named settings, edited as a nested form. */
  | "object"
  /** An array of objects (or of tagged variants): a repeatable form, one per entry. */
  | "object-list"
  /** String keys mapping to values of one schema: key/value rows. */
  | "map"
  /** An internally tagged enum: pick the variant, then fill in that variant's fields. */
  | "variant"
  /**
   * An externally tagged enum (serde's default representation): each choice
   * is either a bare string (`"negotiate"`) or a one-key object carrying that
   * choice's value (`{"bytes": 4096}`). Pick the choice, then fill in its value.
   */
  | "choice"
  /** A shape this interface cannot describe. Shown read-only, with a note. */
  | "unsupported";

/** The kinds that are one value in one labelable control. */
export const SCALAR_KINDS: ReadonlySet<FieldKind> = new Set<FieldKind>([
  "boolean",
  "enum",
  "integer",
  "number",
  "string",
  "duration",
  "bytes",
  "secret",
]);

/** The kinds rendered as a nested form, and so given the full width of a row. */
export const STRUCTURED_KINDS: ReadonlySet<FieldKind> = new Set<FieldKind>([
  "object",
  "object-list",
  "map",
  "variant",
  "choice",
  "unsupported",
]);

/** One permitted value of an `enum` or `enum-list`, already labelled. */
export interface SettingOption {
  value: string;
  label: string;
  /** The value's own doc comment, when the schema has one (a `oneOf` of `const`s does). */
  description?: string;
}

export interface SettingField {
  /** Dotted path within the section: `password.enabled`. */
  path: string;
  /** Dotted path within the whole configuration: `auth.password.enabled`. */
  fullPath: string;
  key: string;
  label: string;
  /** First paragraph of the field's doc comment — the inline hint. */
  summary?: string;
  /** The whole doc comment, shown behind a disclosure when it says more than the summary. */
  description?: string;
  kind: FieldKind;
  /** For `enum` and `enum-list`: the permitted values, already labelled. */
  options?: SettingOption[];
  /** The schema's own default, when it has one. `undefined` means the field is required. */
  defaultValue?: JsonValue;
  hasDefault: boolean;
  required: boolean;
  /** Placeholder/unit hint for the text kinds that carry a unit (`30s`, `50M`). */
  unitHint?: string;
  minimum?: number;
  maximum?: number;
  /** A `const` in the schema — the discriminator of a tagged enum. Shown, never edited. */
  readOnly: boolean;
  /**
   * `config.update` would accept a change to this setting. The server's own
   * answer where it gives one (`ConfigSettingInfo.editable`); otherwise
   * inferred from the origin, which is what it was before the field existed.
   */
  editable: boolean;
  /**
   * Set at install, never administered here (decision 0010): the server's
   * `ConfigSettingInfo.bootstrap`, or the whole section being bootstrap.
   */
  bootstrap: boolean;
  /**
   * The schema node the field was built from, `$ref`s followed and `Option<T>`
   * unwrapped. The structured kinds build their nested forms from it.
   */
  schema?: JsonSchemaNode;
  /** `$defs`, for the `$ref`s inside {@link schema}. */
  defs?: Record<string, JsonSchemaNode>;
  /** The schema admits `null` (an `Option<T>`), so a nested form may be left unset. */
  nullable?: boolean;
}

export interface SettingGroup {
  /** Dotted path within the section; `""` for the section itself. */
  path: string;
  label: string;
  summary?: string;
  description?: string;
  fields: SettingField[];
  groups: SettingGroup[];
}

/** What the API sends in place of a secret's value. Never the secret itself. */
export const SECRET_MARKER = "$secret";

export function isSecretValue(value: unknown): boolean {
  return (
    typeof value === "object" &&
    value !== null &&
    !Array.isArray(value) &&
    (value as Record<string, unknown>)[SECRET_MARKER] === true
  );
}

function isRecord(value: unknown): value is Record<string, JsonValue> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** Structural equality over JSON values; object key order does not count. */
export function deepEqual(a: unknown, b: unknown): boolean {
  if (a === b) return true;
  if (typeof a !== typeof b) return false;
  if (Array.isArray(a) || Array.isArray(b)) {
    if (!Array.isArray(a) || !Array.isArray(b) || a.length !== b.length) return false;
    return a.every((item, i) => deepEqual(item, b[i]));
  }
  if (isRecord(a) && isRecord(b)) {
    const aKeys = Object.keys(a);
    const bKeys = Object.keys(b);
    if (aKeys.length !== bKeys.length) return false;
    return aKeys.every((k) => k in b && deepEqual(a[k], b[k]));
  }
  return false;
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/** Reads a dotted path out of a document. `undefined` when any step is missing. */
export function getPath(document: JsonValue | undefined, path: string): JsonValue | undefined {
  if (path === "") return document;
  let current: JsonValue | undefined = document;
  for (const key of path.split(".")) {
    if (!isRecord(current)) return undefined;
    current = current[key];
  }
  return current;
}

/** Removes a dotted path, returning a new document. Missing paths are left alone. */
export function deletePath(document: JsonValue, path: string): JsonValue {
  if (!isRecord(document) || path === "") return document;
  const [head, ...rest] = path.split(".");
  if (!(head in document)) return document;
  const base = { ...document };
  if (rest.length === 0) delete base[head];
  else base[head] = deletePath(base[head], rest.join("."));
  return base;
}

/** Sets a dotted path, returning a new document. Intermediate objects are created as needed. */
export function setPath(document: JsonValue, path: string, value: JsonValue): JsonValue {
  if (path === "") return value;
  const [head, ...rest] = path.split(".");
  const base: Record<string, JsonValue> = isRecord(document) ? { ...document } : {};
  base[head] = rest.length === 0 ? value : setPath(base[head] ?? {}, rest.join("."), value);
  return base;
}

// ---------------------------------------------------------------------------
// Labels and documentation
// ---------------------------------------------------------------------------

const ACRONYMS: Record<string, string> = {
  api: "API",
  ca: "CA",
  cidr: "CIDR",
  cors: "CORS",
  db: "DB",
  dsn: "DSN",
  edu: "EDU",
  edus: "EDUs",
  gcs: "GCS",
  id: "ID",
  idp: "IdP",
  ip: "IP",
  ips: "IPs",
  mas: "MAS",
  oidc: "OIDC",
  os: "OS",
  otlp: "OTLP",
  pdu: "PDU",
  pdus: "PDUs",
  pem: "PEM",
  san: "SAN",
  sso: "SSO",
  tls: "TLS",
  ttl: "TTL",
  url: "URL",
  urls: "URLs",
  uri: "URI",
  yaml: "YAML",
};

/** `enable_registration` → "Enable registration"; `ip_range_blocklist` → "IP range blocklist". */
export function humanizeKey(key: string): string {
  const words = key.split(/[_\s]+/).filter(Boolean);
  return words
    .map((word, index) => {
      const acronym = ACRONYMS[word.toLowerCase()];
      if (acronym) return acronym;
      if (index === 0) return word.charAt(0).toUpperCase() + word.slice(1);
      return word;
    })
    .join(" ");
}

/**
 * Rustdoc leaks into the schema: `schemars` copies the doc comment verbatim,
 * intra-doc links and all. Strip the link syntax and collapse whitespace so a
 * hint reads as prose rather than as source.
 */
export function cleanDoc(text: string | undefined): string | undefined {
  if (!text) return undefined;
  const cleaned = text
    .replace(/\[`([^`\]]+)`\]/g, "$1")
    .replace(/`([^`]+)`/g, "$1")
    .replace(/\s+/g, " ")
    .trim();
  return cleaned || undefined;
}

/** The first sentence or two of a doc comment — enough for an inline hint. */
export function summarize(text: string | undefined, limit = 180): string | undefined {
  const cleaned = cleanDoc(text);
  if (!cleaned) return undefined;
  if (cleaned.length <= limit) return cleaned;
  const firstSentence = /^(.*?[.!?])\s/.exec(cleaned);
  if (firstSentence && firstSentence[1].length <= limit) return firstSentence[1];
  const cut = cleaned.slice(0, limit);
  const lastSpace = cut.lastIndexOf(" ");
  return `${(lastSpace > 40 ? cut.slice(0, lastSpace) : cut).trimEnd()}…`;
}

// ---------------------------------------------------------------------------
// Schema walking
// ---------------------------------------------------------------------------

function typesOf(node: JsonSchemaNode): string[] {
  if (!node.type) return [];
  return Array.isArray(node.type) ? node.type : [node.type];
}

/**
 * `Option<T>` reaches the schema as `anyOf: [T, {type: "null"}]` (or a
 * two-element `type` array). Unwrap it so an optional duration still renders
 * as a duration rather than as raw JSON.
 */
function unwrapNullable(
  node: JsonSchemaNode,
  defs: Record<string, JsonSchemaNode>,
): JsonSchemaNode {
  const variants = node.anyOf ?? node.oneOf;
  if (!variants || variants.length !== 2) return node;
  const nonNull = variants.filter((v) => !typesOf(v).includes("null"));
  if (nonNull.length !== 1) return node;
  const { anyOf: _a, oneOf: _o, ...rest } = node;
  return { ...resolveRef(nonNull[0], defs), ...rest };
}

const DURATION_HINT = /^a duration:/i;
const BYTES_HINT = /^a byte size:/i;

function refName(node: JsonSchemaNode): string {
  return node.$ref ? node.$ref.replace(/^#\/(\$defs|definitions)\//, "") : "";
}

/**
 * The scalar kinds the two `hs-config` newtypes reach us as. Both serialise as
 * "a string with a unit, or a plain integer" (`crates/hs-config/src/duration.rs`
 * and `size.rs`), which is indistinguishable from `string | integer` without
 * one of these tells.
 */
function unitKind(raw: JsonSchemaNode, resolved: JsonSchemaNode): "duration" | "bytes" | null {
  // The real schema marks both newtypes outright. It is the only tell that
  // survives an `Option<Duration>`, whose `$ref` sits inside an `anyOf` and
  // whose description is the field's own rather than the type's.
  if (resolved["x-duration"] === true) return "duration";
  if (resolved["x-bytesize"] === true) return "bytes";
  const ref = refName(raw).toLowerCase();
  if (ref.includes("duration")) return "duration";
  if (ref.includes("bytesize") || ref.includes("byte_size")) return "bytes";
  if (resolved.format === "duration") return "duration";
  if (resolved.format === "byte-size" || resolved.format === "bytes") return "bytes";
  const description = resolved.description ?? "";
  if (DURATION_HINT.test(description)) return "duration";
  if (BYTES_HINT.test(description)) return "bytes";
  return null;
}

function isSecretNode(node: JsonSchemaNode): boolean {
  return node["x-secret"] === true || node.writeOnly === true || node.format === "password";
}

function isNullOnly(node: JsonSchemaNode): boolean {
  const types = typesOf(node);
  return types.length === 1 && types[0] === "null";
}

/** `Option<T>`: a `null` variant, or `null` among the node's own types. */
function admitsNull(node: JsonSchemaNode, defs: Record<string, JsonSchemaNode>): boolean {
  if (typesOf(node).includes("null")) return true;
  const variants = node.anyOf ?? node.oneOf ?? [];
  return variants.some((v) => isNullOnly(resolveRef(v, defs)));
}

/**
 * The permitted values of a string enum, in either spelling: a plain `enum`
 * array, or what `schemars` emits for a documented Rust enum of unit variants
 * — a `oneOf` of `{type: "string", const: …}`, one per variant, each carrying
 * that variant's doc comment (`LogLevel`, `ThumbnailMethod`,
 * `ListenerResource`). `null` for anything else.
 */
function enumOptions(
  node: JsonSchemaNode,
  defs: Record<string, JsonSchemaNode>,
): SettingOption[] | null {
  if (node.enum && node.enum.length > 0) {
    if (!node.enum.every((v) => typeof v === "string")) return null;
    return (node.enum as string[]).map((v) => ({ value: v, label: humanizeKey(v) }));
  }
  if (Object.keys(node.properties ?? {}).length > 0) return null;
  const variants = node.oneOf ?? node.anyOf;
  if (!variants || variants.length === 0) return null;
  const options: SettingOption[] = [];
  for (const variant of variants) {
    const resolved = resolveRef(variant, defs);
    if (isNullOnly(resolved)) continue;
    if (typeof resolved.const === "string") {
      options.push({
        value: resolved.const,
        label: humanizeKey(resolved.const),
        description: summarize(resolved.description),
      });
    } else if (resolved.enum?.length && resolved.enum.every((v) => typeof v === "string")) {
      options.push(
        ...(resolved.enum as string[]).map((v) => ({ value: v, label: humanizeKey(v) })),
      );
    } else {
      return null;
    }
  }
  return options.length > 0 ? options : null;
}

/** One variant of an internally tagged enum. */
export interface VariantOption extends SettingOption {
  /** The variant's object schema, the tag property included. */
  node: JsonSchemaNode;
}

/** An internally tagged enum: which property is the tag, and the variants it selects. */
export interface VariantInfo {
  /** The property every variant pins to a `const`: `backend` for `media.storage`. */
  tag: string;
  options: VariantOption[];
}

/**
 * Recognises an internally tagged Rust enum (`#[serde(tag = "backend")]`):
 * a `oneOf` of object variants that all pin one property to a string
 * `const`. `media.storage` is one (`local`, `s3`, `gcs`, `azure`), and so is
 * the `storage` section itself.
 */
export function variantInfo(
  node: JsonSchemaNode | undefined,
  defs: Record<string, JsonSchemaNode>,
): VariantInfo | null {
  if (!node || Object.keys(node.properties ?? {}).length > 0) return null;
  const variants = (node.oneOf ?? node.anyOf ?? [])
    .map((v) => resolveRef(v, defs))
    .filter((v) => !isNullOnly(v));
  if (variants.length === 0) return null;
  if (variants.some((v) => Object.keys(v.properties ?? {}).length === 0)) return null;
  const constOf = (variant: JsonSchemaNode, key: string): unknown => {
    const prop = variant.properties?.[key];
    return prop ? resolveRef(prop, defs).const : undefined;
  };
  const tag = Object.keys(variants[0].properties ?? {}).find((key) =>
    variants.every((v) => typeof constOf(v, key) === "string"),
  );
  if (!tag) return null;
  return {
    tag,
    options: variants.map((variant) => {
      const value = constOf(variant, tag) as string;
      return {
        value,
        label: humanizeKey(value),
        description: summarize(variant.description),
        node: variant,
      };
    }),
  };
}

/** One choice of an externally tagged enum. */
export interface ChoiceOption extends SettingOption {
  /**
   * The schema of the value this choice carries (`{"bytes": N}`'s `N`), or
   * `undefined` for a choice that is just its name (`"negotiate"`).
   */
  payload?: JsonSchemaNode;
}

/**
 * Recognises an externally tagged Rust enum — serde's default representation,
 * which `schemars` renders as a `oneOf` whose unit variants are string
 * `const`s and whose data-carrying variants are objects with exactly one
 * required property, the variant's name. `media.scanning.icap.preview`
 * (`PreviewMode`: `"negotiate"`, `"off"` or `{"bytes": N}`) is one. At least
 * one variant must carry data; an enum of names only is an `enum`, and one of
 * internally tagged objects is a `variant` (both are tried first).
 */
export function choiceInfo(
  node: JsonSchemaNode | undefined,
  defs: Record<string, JsonSchemaNode>,
): ChoiceOption[] | null {
  if (!node || Object.keys(node.properties ?? {}).length > 0) return null;
  const variants = (node.oneOf ?? node.anyOf ?? [])
    .map((v) => resolveRef(v, defs))
    .filter((v) => !isNullOnly(v));
  if (variants.length === 0) return null;
  const options: ChoiceOption[] = [];
  let carriesData = false;
  for (const variant of variants) {
    if (typeof variant.const === "string") {
      options.push({
        value: variant.const,
        label: humanizeKey(variant.const),
        description: summarize(variant.description),
      });
      continue;
    }
    if (variant.enum?.length && variant.enum.every((v) => typeof v === "string")) {
      options.push(...(variant.enum as string[]).map((v) => ({ value: v, label: humanizeKey(v) })));
      continue;
    }
    const properties = variant.properties ?? {};
    const keys = Object.keys(properties);
    if (
      keys.length === 1 &&
      (variant.required ?? []).includes(keys[0]) &&
      resolveRef(properties[keys[0]], defs).const === undefined
    ) {
      carriesData = true;
      options.push({
        value: keys[0],
        label: humanizeKey(keys[0]),
        description: summarize(variant.description),
        payload: properties[keys[0]],
      });
      continue;
    }
    return null;
  }
  return carriesData ? options : null;
}

/** The choice a value of an externally tagged enum is: by its string, or by its one key. */
export function chosenChoice(
  field: SettingField,
  value: JsonValue | undefined,
): ChoiceOption | undefined {
  const options = choiceInfo(field.schema, field.defs ?? {});
  if (!options) return undefined;
  if (typeof value === "string") return options.find((o) => o.value === value && !o.payload);
  if (isRecord(value)) {
    const keys = Object.keys(value);
    if (keys.length === 1) return options.find((o) => o.value === keys[0] && o.payload);
  }
  return undefined;
}

/** The value a data-carrying choice holds, as a field of its own: `bytes` for `{"bytes": N}`. */
export function choicePayloadField(
  field: SettingField,
  option: ChoiceOption,
  value: JsonValue | undefined,
): SettingField {
  return makeField({
    key: option.value,
    raw: option.payload ?? {},
    defs: field.defs ?? {},
    path: `${field.path}.${option.value}`,
    fullPath: `${field.fullPath}.${option.value}`,
    value: isRecord(value) ? value[option.value] : undefined,
    required: true,
    editable: field.editable,
    bootstrap: field.bootstrap,
    label: option.label,
  });
}

/**
 * The value picking `choice` produces: the bare name for a choice that is only
 * a name, or `{choice: blank}` for one that carries a value, keeping the value
 * already there when the choice does not change.
 */
export function switchChoice(
  field: SettingField,
  value: JsonValue | undefined,
  choice: string,
): JsonValue {
  const option = choiceInfo(field.schema, field.defs ?? {})?.find((o) => o.value === choice);
  if (!option) return value ?? choice;
  if (!option.payload) return choice;
  if (isRecord(value) && choice in value) return value;
  const payload = choicePayloadField(field, option, undefined);
  const blank =
    payload.hasDefault && payload.defaultValue !== undefined
      ? cloneJson(payload.defaultValue)
      : emptyValue(payload);
  return { [choice]: blank };
}

/** A map's value schema: an object with no fixed properties but `additionalProperties: {…}`. */
function mapValueSchema(node: JsonSchemaNode): JsonSchemaNode | null {
  if (Object.keys(node.properties ?? {}).length > 0) return null;
  const extra = node.additionalProperties;
  return typeof extra === "object" && extra !== null ? extra : null;
}

/** The shapes a nested form can render: an object, a tagged variant, a map. */
function isFormShape(node: JsonSchemaNode, defs: Record<string, JsonSchemaNode>): boolean {
  return (
    Object.keys(node.properties ?? {}).length > 0 ||
    variantInfo(node, defs) !== null ||
    mapValueSchema(node) !== null
  );
}

/** What an array's entries are, and so which list control edits it. */
function listKind(
  items: JsonSchemaNode | undefined,
  defs: Record<string, JsonSchemaNode>,
): FieldKind {
  if (!items) return "unsupported";
  const item = unwrapNullable(resolveRef(items, defs), defs);
  if (enumOptions(item, defs)) return "enum-list";
  const types = typesOf(item);
  if (types.includes("string")) return "string-list";
  if (types.includes("integer") || types.includes("number")) return "number-list";
  if (isFormShape(item, defs)) return "object-list";
  return "unsupported";
}

function classify(
  raw: JsonSchemaNode,
  resolved: JsonSchemaNode,
  value: JsonValue | undefined,
  info: ConfigSettingInfo | undefined,
  defs: Record<string, JsonSchemaNode>,
): FieldKind {
  if (info?.secret || isSecretValue(value) || isSecretNode(resolved)) return "secret";

  const unit = unitKind(raw, resolved);
  if (unit) return unit;

  if (enumOptions(resolved, defs)) return "enum";

  const types = typesOf(resolved).filter((t) => t !== "null");
  if (types.includes("boolean")) return "boolean";
  if (types.includes("array")) return listKind(resolved.items, defs);
  if (types.includes("integer")) return "integer";
  if (types.includes("number")) return "number";
  if (types.includes("string")) return "string";
  if (Object.keys(resolved.properties ?? {}).length > 0) return "object";
  if (variantInfo(resolved, defs)) return "variant";
  if (mapValueSchema(resolved)) return "map";
  if (choiceInfo(resolved, defs)) return "choice";
  return "unsupported";
}

const UNIT_HINTS: Partial<Record<FieldKind, string>> = {
  duration: "e.g. 500ms, 30s, 5m, 1h, 7d",
  bytes: "e.g. 50M, 512K, 1GiB",
};

interface FieldSpec {
  key: string;
  /** The property's schema as written, before any `$ref` is followed. */
  raw: JsonSchemaNode;
  defs: Record<string, JsonSchemaNode>;
  path: string;
  fullPath: string;
  value: JsonValue | undefined;
  required: boolean;
  info?: ConfigSettingInfo;
  /** Overrides {@link ConfigSettingInfo.editable}: a nested field inherits its setting's. */
  editable?: boolean;
  /** Overrides {@link ConfigSettingInfo.bootstrap}: a nested field inherits its setting's. */
  bootstrap?: boolean;
  /** Overrides the label the key or the schema's `title` would give. */
  label?: string;
}

function makeField(spec: FieldSpec): SettingField {
  const { key, raw, defs, info } = spec;
  const base = resolveRef(raw, defs);
  const resolved = unwrapNullable(base, defs);
  const kind = classify(raw, resolved, spec.value, info, defs);
  const description = cleanDoc(resolved.description);
  const summary = summarize(resolved.description);
  const options =
    kind === "enum"
      ? enumOptions(resolved, defs)
      : kind === "enum-list"
        ? enumOptions(unwrapNullable(resolveRef(resolved.items ?? {}, defs), defs), defs)
        : null;
  return {
    path: spec.path,
    fullPath: spec.fullPath,
    key,
    label: spec.label ?? (resolved.title ? cleanDoc(resolved.title)! : humanizeKey(key)),
    summary,
    description: description && description !== summary ? description : undefined,
    kind,
    options: options ?? undefined,
    defaultValue: resolved.default,
    hasDefault: resolved.default !== undefined,
    required: spec.required,
    unitHint: UNIT_HINTS[kind],
    minimum: resolved.minimum,
    maximum: resolved.maximum,
    readOnly: resolved.const !== undefined,
    editable: spec.editable ?? (info ? info.editable : true),
    bootstrap: spec.bootstrap ?? info?.bootstrap ?? false,
    schema: resolved,
    defs,
    nullable: admitsNull(base, defs),
  };
}

function walk(
  node: JsonSchemaNode,
  defs: Record<string, JsonSchemaNode>,
  prefix: string,
  sectionName: string,
  values: JsonValue | undefined,
  settings: Record<string, ConfigSettingInfo>,
  depth: number,
): { fields: SettingField[]; groups: SettingGroup[] } {
  const fields: SettingField[] = [];
  const groups: SettingGroup[] = [];
  const required = new Set(node.required ?? []);

  for (const [key, child] of Object.entries(node.properties ?? {})) {
    const path = prefix ? `${prefix}.${key}` : key;
    const resolved = unwrapNullable(resolveRef(child, defs), defs);
    const value = getPath(values, path);
    const info = settings[`${sectionName}.${path}`];
    const kind = classify(child, resolved, value, info, defs);

    // A nested object is a group of its own rows, each saved on its own —
    // merge patch merges objects key by key, so that is exact. Depth 4 is
    // well past anything in `hs_config::Config`; the guard is against a
    // schema that refs itself, not against a deep config. Past it, the
    // object is still edited, as one nested form.
    if (kind === "object" && depth < 4) {
      const nested = walk(resolved, defs, path, sectionName, values, settings, depth + 1);
      groups.push({
        path,
        label: resolved.title ? cleanDoc(resolved.title)! : humanizeKey(key),
        summary: summarize(resolved.description),
        description: cleanDoc(resolved.description),
        ...nested,
      });
      continue;
    }

    fields.push(
      makeField({
        key,
        raw: child,
        defs,
        path,
        fullPath: `${sectionName}.${path}`,
        value,
        required: required.has(key),
        info,
      }),
    );
  }

  return { fields, groups };
}

// ---------------------------------------------------------------------------
// Nested forms: the settings inside one setting's value
// ---------------------------------------------------------------------------

/**
 * The variant a tagged value is, by its tag. `undefined` when the value names
 * no variant this schema has (or is not an object at all).
 */
export function chosenVariant(
  field: SettingField,
  value: JsonValue | undefined,
): VariantOption | undefined {
  const info = variantInfo(field.schema, field.defs ?? {});
  if (!info || !isRecord(value)) return undefined;
  return info.options.find((option) => option.value === value[info.tag]);
}

function propertyFieldsOf(
  node: JsonSchemaNode,
  field: SettingField,
  value: JsonValue | undefined,
  skip?: string,
): SettingField[] {
  const required = new Set(node.required ?? []);
  return Object.entries(node.properties ?? {})
    .filter(([key]) => key !== skip)
    .map(([key, raw]) =>
      makeField({
        key,
        raw,
        defs: field.defs ?? {},
        path: `${field.path}.${key}`,
        fullPath: `${field.fullPath}.${key}`,
        value: isRecord(value) ? value[key] : undefined,
        required: required.has(key),
        editable: field.editable,
        bootstrap: field.bootstrap,
      }),
    );
}

/**
 * The settings inside an `object` value, or inside a `variant` value's
 * chosen variant (its tag excluded — the variant picker edits that).
 */
export function propertyFields(field: SettingField, value: JsonValue | undefined): SettingField[] {
  if (field.kind === "variant") {
    const info = variantInfo(field.schema, field.defs ?? {});
    const chosen = chosenVariant(field, value);
    return chosen && info ? propertyFieldsOf(chosen.node, field, value, info.tag) : [];
  }
  return field.schema ? propertyFieldsOf(field.schema, field, value) : [];
}

/** `Listener` → "Listener"; `OidcProviderConfig` → "OIDC provider"; `ThumbnailSize` → "Thumbnail size". */
export function nounFromTypeName(name: string): string {
  const words = name
    .replace(/Config$/, "")
    .split(/(?<=[a-z0-9])(?=[A-Z])/)
    .filter(Boolean);
  if (words.length === 0) return "Entry";
  return humanizeKey(words.map((w) => w.toLowerCase()).join("_"));
}

/** What one entry of an `object-list` is called: "Listener", "Thumbnail size", "Entry". */
export function entryNoun(field: SettingField): string {
  const items = field.schema?.items;
  const name = items ? refName(items) : "";
  return name ? nounFromTypeName(name) : "Entry";
}

/** One entry of a list, as a field of its own: its kind, its nested form. */
export function itemField(
  field: SettingField,
  index: number,
  value: JsonValue | undefined,
): SettingField {
  return makeField({
    key: String(index),
    raw: field.schema?.items ?? {},
    defs: field.defs ?? {},
    path: `${field.path}.${index}`,
    fullPath: `${field.fullPath}[${index}]`,
    value,
    required: true,
    editable: field.editable,
    bootstrap: field.bootstrap,
    label: `${entryNoun(field)} ${index + 1}`,
  });
}

/** The value under one key of a `map`, as a field of its own. */
export function mapEntryField(
  field: SettingField,
  key: string,
  value: JsonValue | undefined,
): SettingField {
  const extra = field.schema?.additionalProperties;
  return makeField({
    key,
    raw: typeof extra === "object" && extra !== null ? extra : {},
    defs: field.defs ?? {},
    path: `${field.path}.${key}`,
    fullPath: `${field.fullPath}.${key}`,
    value,
    // The key being there is what makes the entry exist; its value is not
    // "required" in any sense worth marking.
    required: false,
    editable: field.editable,
    bootstrap: field.bootstrap,
    label: key || "New entry",
  });
}

function cloneJson<T extends JsonValue>(value: T): T {
  return structuredClone(value);
}

/**
 * A new, blank value for a field: what "Add a listener" puts in the list.
 *
 * Every property with a schema default gets that default, so the new entry
 * shows what the server would assume; every *required* property without one
 * starts empty, so the operator can see what still has to be filled in (and
 * the server says so if it is not); optional properties without a default are
 * left out, which is what not setting them means.
 */
export function emptyValue(field: SettingField): JsonValue {
  switch (field.kind) {
    case "boolean":
      return false;
    case "enum":
      return field.options?.[0]?.value ?? "";
    case "string-list":
    case "number-list":
    case "enum-list":
    case "object-list":
      return [];
    case "map":
      return {};
    case "object":
      return objectSkeleton(propertyFields(field, undefined));
    case "variant": {
      const info = variantInfo(field.schema, field.defs ?? {});
      const first = info?.options[0];
      return info && first ? switchVariant(field, undefined, first.value) : {};
    }
    case "choice": {
      const first = choiceInfo(field.schema, field.defs ?? {})?.[0];
      return first ? switchChoice(field, undefined, first.value) : "";
    }
    default:
      return "";
  }
}

function objectSkeleton(fields: SettingField[]): Record<string, JsonValue> {
  const out: Record<string, JsonValue> = {};
  for (const sub of fields) {
    if (sub.readOnly && sub.schema?.const !== undefined) {
      out[sub.key] = cloneJson(sub.schema.const);
    } else if (sub.hasDefault) {
      if (sub.defaultValue !== null && sub.defaultValue !== undefined) {
        out[sub.key] = cloneJson(sub.defaultValue);
      }
    } else if (sub.required) {
      out[sub.key] = emptyValue(sub);
    }
  }
  return out;
}

/**
 * The value a variant picker produces when the operator picks `tag`: the new
 * variant's blank value, keeping whatever the old value said for a property
 * the new variant also has (`bucket`, moving from S3 to GCS).
 */
export function switchVariant(
  field: SettingField,
  value: JsonValue | undefined,
  tag: string,
): JsonValue {
  const info = variantInfo(field.schema, field.defs ?? {});
  const option = info?.options.find((o) => o.value === tag);
  if (!info || !option) return value ?? {};
  const fields = propertyFieldsOf(option.node, field, undefined, info.tag);
  const next: Record<string, JsonValue> = { [info.tag]: tag, ...objectSkeleton(fields) };
  if (isRecord(value)) {
    for (const sub of fields) {
      if (sub.key in value && !isSecretValue(value[sub.key])) next[sub.key] = value[sub.key];
    }
  }
  return next;
}

/**
 * A few words that tell one entry of a list from another — its required
 * scalar settings, in schema order: "8008" for a listener, "google ·
 * https://accounts.google.com · matrix" for an OIDC provider.
 */
export function entrySummary(
  field: SettingField,
  value: JsonValue | undefined,
): string | undefined {
  if (!isRecord(value)) return value === undefined ? undefined : formatValue(value);
  const fields = propertyFields(field, value);
  const scalar = (f: SettingField) =>
    f.kind !== "secret" && SCALAR_KINDS.has(f.kind) && value[f.key] !== undefined;
  const picked = fields.filter((f) => f.required && scalar(f));
  const chosen = (picked.length > 0 ? picked : fields.filter(scalar)).slice(0, 3);
  const parts = chosen
    .map((f) => value[f.key])
    .filter((v) => v !== null && v !== "")
    .map((v) => formatValue(v));
  if (field.kind === "variant") {
    const variant = chosenVariant(field, value);
    if (variant) parts.unshift(variant.label);
  }
  return parts.length > 0 ? parts.join(" · ") : undefined;
}

/** The member of a hidden secret that says where it came from (docs/rfcs/0020). */
export const SECRET_FROM = "$from";

/**
 * A setting's whole-configuration JSON Pointer from its dotted `fullPath`:
 * `auth.oidc_providers` → `/auth/oidc_providers`, `a.list[2].b` → `/a/list/2/b`.
 */
export function pointerOf(fullPath: string): string {
  return `/${fullPath
    .replace(/\[(\d+)\]/g, ".$1")
    .split(".")
    .filter(Boolean)
    .map((token) => token.replace(/~/g, "~0").replace(/\//g, "~1"))
    .join("/")}`;
}

/**
 * Marks every hidden secret inside `value` with where it is stored now
 * (`{"$secret": true, "$from": "/auth/oidc_providers/1/client_secret"}`),
 * `value` being what is stored at `pointer`. The list editor does this to
 * every entry before it moves or removes one, so each untouched secret still
 * names its own origin after the entries around it have shifted, and the
 * server puts back the right one (docs/rfcs/0020). A secret already marked
 * keeps its mark: it names where it was first shown.
 */
export function markSecretOrigins(value: JsonValue, pointer: string): JsonValue {
  if (isSecretValue(value)) {
    const record = value as Record<string, JsonValue>;
    return SECRET_FROM in record ? value : { [SECRET_MARKER]: true, [SECRET_FROM]: pointer };
  }
  if (Array.isArray(value)) {
    return value.map((item, index) => markSecretOrigins(item, `${pointer}/${index}`));
  }
  if (isRecord(value)) {
    const out: Record<string, JsonValue> = {};
    for (const [key, child] of Object.entries(value)) {
      out[key] = markSecretOrigins(
        child,
        `${pointer}/${key.replace(/~/g, "~0").replace(/\//g, "~1")}`,
      );
    }
    return out;
  }
  return value;
}

/** Whether a value holds a redacted secret anywhere inside it. */
export function containsSecret(value: JsonValue | undefined): boolean {
  if (isSecretValue(value)) return true;
  if (Array.isArray(value)) return value.some(containsSecret);
  if (isRecord(value)) return Object.values(value).some(containsSecret);
  return false;
}

/**
 * The field a validation error belongs to. The server names the exact
 * setting that failed — `listeners.0.port`, or `oidc_providers[1].issuer` in
 * `hs-config`'s own spelling — but a list is edited as one setting, so the
 * error lands on the list's row. Longest match wins, so `password.enabled`
 * lands on itself and not on a `password` that is not a field.
 */
export function ownerFieldPath(path: string, fieldPaths: Iterable<string>): string | undefined {
  return ownerOf(path, fieldPaths);
}

/**
 * Where inside a setting an error points, in words: the part of
 * `thumbnail_sizes.2.width` after the setting is "entry 3, width".
 */
export function describeSubPath(rest: string): string {
  return rest
    .replace(/\[(\d+)\]/g, ".$1")
    .split(".")
    .filter(Boolean)
    .map((part) => (/^\d+$/.test(part) ? `entry ${Number(part) + 1}` : humanizeKey(part)))
    .map((part, index) => {
      if (index === 0) return part.charAt(0).toUpperCase() + part.slice(1);
      // "Width" reads as "width" mid-sentence; "IP range" and "IdP ID" stay.
      return /^[A-Z][a-z]/.test(part) && !/^IdP/.test(part)
        ? part.charAt(0).toLowerCase() + part.slice(1)
        : part;
    })
    .join(", ");
}

function ownerOf(path: string, fieldPaths: Iterable<string>): string | undefined {
  let best: string | undefined;
  for (const candidate of fieldPaths) {
    const owns =
      path === candidate || path.startsWith(`${candidate}.`) || path.startsWith(`${candidate}[`);
    if (owns && (best === undefined || candidate.length > best.length)) best = candidate;
  }
  return best;
}

/**
 * Picks the live variant of an internally tagged enum.
 *
 * `storage` is a Rust enum with `#[serde(tag = "backend")]`, so it reaches
 * the schema as a `oneOf` of three object variants, each pinning `backend`
 * to a `const`. Rendering the union would be meaningless; rendering the
 * variant the server is actually running is exactly what an operator wants
 * to see. Falls back to the first object variant when nothing matches.
 */
function pickVariant(
  node: JsonSchemaNode,
  defs: Record<string, JsonSchemaNode>,
  values: JsonValue | undefined,
): JsonSchemaNode {
  const variants = node.oneOf ?? node.anyOf;
  if (!variants || Object.keys(node.properties ?? {}).length > 0) return node;
  const resolvedVariants = variants
    .map((v) => resolveRef(v, defs))
    .filter((v) => Object.keys(v.properties ?? {}).length > 0);
  if (resolvedVariants.length === 0) return node;
  const matched = resolvedVariants.find((variant) =>
    Object.entries(variant.properties ?? {}).some(
      ([key, prop]) => prop.const !== undefined && deepEqual(getPath(values, key), prop.const),
    ),
  );
  const { oneOf: _o, anyOf: _a, ...rest } = node;
  return { ...(matched ?? resolvedVariants[0]), ...rest };
}

/**
 * The form model for one section: its fields, and one subgroup per nested
 * object. Sections the schema does not describe come back empty rather than
 * throwing, so a server that grows a section this build has never heard of
 * still renders a page that says so.
 */
export function buildSectionModel(
  schema: ConfigSchemaModel,
  sectionName: string,
  values: JsonValue | undefined,
): SettingGroup {
  const raw = schema.root.properties?.[sectionName];
  if (!raw) {
    return { path: "", label: humanizeKey(sectionName), fields: [], groups: [] };
  }
  const resolved = pickVariant(
    unwrapNullable(resolveRef(raw, schema.defs), schema.defs),
    schema.defs,
    values,
  );
  const nested = walk(resolved, schema.defs, "", sectionName, values, schema.settings ?? {}, 0);
  return {
    path: "",
    label: resolved.title ? cleanDoc(resolved.title)! : humanizeKey(sectionName),
    summary: summarize(resolved.description),
    description: cleanDoc(resolved.description),
    ...nested,
  };
}

/** Every field in a group tree, depth first — the order the form renders them in. */
export function flattenFields(group: SettingGroup): SettingField[] {
  return [...group.fields, ...group.groups.flatMap(flattenFields)];
}

// ---------------------------------------------------------------------------
// Edits
// ---------------------------------------------------------------------------

/**
 * Pending edits for one section, keyed by the setting's path within it.
 * `null` is not "set to null" — it is RFC 7396's removal, which the server
 * reads as "reset this to the schema default"
 * (`crates/hs-config/src/document.rs`'s module doc).
 */
export type Draft = Record<string, JsonValue | null>;

/** The RFC 7396 merge patch a draft produces: nested objects, `null` for a reset. */
export function buildMergePatch(draft: Draft): Record<string, JsonValue> {
  let patch: JsonValue = {};
  for (const [path, value] of Object.entries(draft)) {
    patch = setPath(patch, path, value as JsonValue);
  }
  return isRecord(patch) ? patch : {};
}

/**
 * The document a draft would produce, for `POST /config/validate` — the same
 * merge the server will do, done locally so "check without saving" can show
 * the real answer before anything is written.
 */
export function applyDraft(values: JsonValue | undefined, draft: Draft): JsonValue {
  let next: JsonValue = values ?? {};
  for (const [path, value] of Object.entries(draft)) {
    next = value === null ? deletePath(next, path) : setPath(next, path, value);
  }
  return next;
}

export interface ChangeEntry {
  path: string;
  label: string;
  field?: SettingField;
  /** What the server has now. */
  from: JsonValue | undefined;
  /** What the operator typed, or `null` for "reset to default". */
  to: JsonValue | null;
  /** True when `to` is a reset; `defaultValue` then says what it reverts to. */
  isReset: boolean;
  defaultValue?: JsonValue;
}

/** The review list: one entry per edit that actually changes something. */
export function changeEntries(
  fields: SettingField[],
  values: JsonValue | undefined,
  draft: Draft,
): ChangeEntry[] {
  const byPath = new Map(fields.map((f) => [f.path, f]));
  return Object.entries(draft)
    .map(([path, to]) => {
      const field = byPath.get(path);
      const from = getPath(values, path);
      return {
        path,
        label: field?.label ?? humanizeKey(path.split(".").pop() ?? path),
        field,
        from,
        to,
        isReset: to === null,
        defaultValue: field?.defaultValue,
      };
    })
    .filter((entry) => {
      // A reset is a change only when the effective value is not already the default.
      if (entry.isReset) {
        return !(entry.field?.hasDefault && deepEqual(entry.from, entry.field.defaultValue));
      }
      return !deepEqual(entry.from, entry.to);
    })
    .sort((a, b) => a.path.localeCompare(b.path));
}

/**
 * Whether a setting's effective value differs from the schema's own default.
 *
 * A field with no `default` keyword is either required or an `Option<T>`; for
 * the latter, `null` and absent both mean "nobody set this", so neither counts
 * as changed.
 */
export function isChangedFromDefault(field: SettingField, value: JsonValue | undefined): boolean {
  if (!field.hasDefault) return value !== undefined && value !== null;
  return !deepEqual(value, field.defaultValue);
}

/**
 * "Changed from default", preferring the server's own answer.
 *
 * The origin map is the authority: a setting whose origin is `default` is one
 * *no layer set*, which is exactly the question. Comparing against the
 * schema's `default` keyword is only the fallback, and a needed one — the
 * duration and byte-size fields carry their defaults in the Rust
 * `Default` impl rather than in `serde(default = ...)`, so they reach the
 * schema with no `default` at all (see the empty Default column for them in
 * `docs/config.md`).
 */
export function isChanged(
  field: SettingField,
  value: JsonValue | undefined,
  origin: ConfigOrigin | undefined,
): boolean {
  if (origin) return origin !== "default";
  return isChangedFromDefault(field, value);
}

// ---------------------------------------------------------------------------
// Validation errors
// ---------------------------------------------------------------------------

/**
 * Lands a server-side validation error on the field it belongs to.
 *
 * The two halves of this system disagree about how to name a setting, and
 * both spellings reach the browser: `hs-config`'s own validator reports
 * dotted paths including the section (`rate_limits.login.per_second`, see
 * `crates/hs-config/src/ratelimit.rs`'s tests), while the admin API's
 * `ValidationError.pointer` is documented as "a JSON Pointer into the body"
 * — and the body of `PATCH /config/{section}` is the section document, so
 * that pointer may or may not repeat the section name. Rather than guess,
 * accept every spelling and strip a leading section name when it is there.
 */
export function normalizeErrorPath(pointer: string, sectionName: string): string {
  const dotted = pointer.startsWith("/")
    ? pointer
        .slice(1)
        .split("/")
        .map((token) => token.replace(/~1/g, "/").replace(/~0/g, "~"))
        .join(".")
    : pointer.replace(/^param:/, "");
  const prefix = `${sectionName}.`;
  if (dotted === sectionName) return "";
  return dotted.startsWith(prefix) ? dotted.slice(prefix.length) : dotted;
}

/**
 * The DOM id the row for `path` carries, so a validation error, a deep link or
 * the search on the index page can all name the same element. Lives here
 * rather than in the component because three unrelated places have to agree
 * on it.
 */
export function settingRowId(path: string): string {
  return `setting-${path.replace(/\./g, "-")}`;
}

export interface FieldError {
  path: string;
  detail: string;
}

/** Groups a `Problem`'s `errors[]` by the field path they land on. */
export function fieldErrorsFor(
  errors: { pointer: string; detail: string }[] | undefined,
  sectionName: string,
): FieldError[] {
  return (errors ?? []).map((e) => ({
    path: normalizeErrorPath(e.pointer, sectionName),
    detail: e.detail,
  }));
}

// ---------------------------------------------------------------------------
// Display
// ---------------------------------------------------------------------------

/** A value as the review list and the read-only rows show it. */
export function formatValue(value: JsonValue | undefined): string {
  if (value === undefined) return "not set";
  if (value === null) return "none";
  if (isSecretValue(value)) return "set, hidden";
  if (typeof value === "boolean") return value ? "on" : "off";
  if (typeof value === "string") return value === "" ? '""' : value;
  if (typeof value === "number") return String(value);
  if (Array.isArray(value)) {
    if (value.length === 0) return "empty list";
    return value.every((v) => typeof v === "string" || typeof v === "number")
      ? value.join(", ")
      : `${value.length} ${value.length === 1 ? "entry" : "entries"}`;
  }
  const entries = Object.entries(value);
  if (entries.length === 0) return "empty";
  // Prose, not JSON: this is what a person reads in the review list.
  return entries.map(([key, v]) => `${humanizeKey(key)}: ${formatValue(v)}`).join(" · ");
}
