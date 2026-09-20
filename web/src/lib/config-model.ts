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
  | "json";

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
  /** For `enum`: the permitted values, already labelled. */
  options?: { value: string; label: string }[];
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
  id: "ID",
  ip: "IP",
  ips: "IPs",
  mas: "MAS",
  oidc: "OIDC",
  os: "OS",
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

function classify(
  raw: JsonSchemaNode,
  resolved: JsonSchemaNode,
  value: JsonValue | undefined,
  info?: ConfigSettingInfo,
): FieldKind {
  if (info?.secret || isSecretValue(value) || isSecretNode(resolved)) return "secret";

  const unit = unitKind(raw, resolved);
  if (unit) return unit;

  const enumValues = resolved.enum;
  if (enumValues && enumValues.length > 0 && enumValues.every((v) => typeof v === "string")) {
    return "enum";
  }

  const types = typesOf(resolved).filter((t) => t !== "null");
  if (types.includes("boolean")) return "boolean";
  if (types.includes("array")) {
    const itemTypes = typesOf(resolved.items ?? {});
    if (itemTypes.includes("string") || (resolved.items?.enum?.length ?? 0) > 0) {
      return "string-list";
    }
    if (itemTypes.includes("integer") || itemTypes.includes("number")) return "number-list";
    return "json";
  }
  if (types.includes("integer")) return "integer";
  if (types.includes("number")) return "number";
  if (types.includes("string")) return "string";
  return "json";
}

/** A node the form should recurse into rather than render as one control. */
function isGroupNode(node: JsonSchemaNode, kind: FieldKind): boolean {
  return kind === "json" && Object.keys(node.properties ?? {}).length > 0;
}

const UNIT_HINTS: Partial<Record<FieldKind, string>> = {
  duration: "e.g. 500ms, 30s, 5m, 1h, 7d",
  bytes: "e.g. 50M, 512K, 1GiB",
};

function buildField(
  key: string,
  raw: JsonSchemaNode,
  resolved: JsonSchemaNode,
  sectionPath: string,
  sectionName: string,
  value: JsonValue | undefined,
  required: boolean,
  info: ConfigSettingInfo | undefined,
): SettingField {
  const kind = classify(raw, resolved, value, info);
  const description = cleanDoc(resolved.description);
  const summary = summarize(resolved.description);
  return {
    path: sectionPath,
    fullPath: `${sectionName}.${sectionPath}`,
    key,
    label: resolved.title ? cleanDoc(resolved.title)! : humanizeKey(key),
    summary,
    description: description && description !== summary ? description : undefined,
    kind,
    options:
      kind === "enum"
        ? (resolved.enum ?? []).map((v) => ({ value: String(v), label: humanizeKey(String(v)) }))
        : undefined,
    defaultValue: resolved.default,
    hasDefault: resolved.default !== undefined,
    required,
    unitHint: UNIT_HINTS[kind],
    minimum: resolved.minimum,
    maximum: resolved.maximum,
    readOnly: resolved.const !== undefined,
    editable: info ? info.editable : true,
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
    const kind = classify(child, resolved, value, info);

    // Depth 4 is well past anything in `hs_config::Config`; the guard is
    // against a schema that refs itself, not against a deep config.
    if (isGroupNode(resolved, kind) && depth < 4) {
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
      buildField(key, child, resolved, path, sectionName, value, required.has(key), info),
    );
  }

  return { fields, groups };
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
      : `${value.length} entries`;
  }
  return JSON.stringify(value);
}
