/**
 * `GET /config/schema` — the one module that knows the wire shape of the
 * configuration schema, and the only one that needs changing when it moves.
 *
 * It has moved once already. This was written against a contract of
 * `{schema, sections, origins}` with `origins` a map of pointer to origin;
 * what track 15 shipped (`components.schemas.ConfigSchema` in
 * `crates/hs-admin/openapi/openapi.yaml`) is richer:
 *
 * ```json
 * {
 *   "schema":   { ... JSON Schema of hs_config::Config, with $defs ... },
 *   "sections": [{ "name": "rate_limits", "reloadable": true, "bootstrap": false,
 *                  "source": "database" }],
 *   "settings": [{ "pointer": "/auth/enable_registration", "section": "auth",
 *                  "origin": "database", "secret": false, "reloadable": false,
 *                  "editable": true }],
 *   "revision": 7
 * }
 * ```
 *
 * `editable` is the improvement worth having: rather than the interface
 * inferring "the API would refuse this" from the origin, the server says so.
 * Both shapes are accepted here, and everything downstream
 * (`lib/config-model.ts`, `pages/config/`) consumes {@link ConfigSchemaModel}
 * and never the raw document, so this stays the whole blast radius.
 *
 * What {@link normalizeConfigSchema} is forgiving about:
 *
 * - **Envelope.** `{schema, …}`, or a bare JSON Schema document with the
 *   metadata missing. A bare document still renders — the reloadable and
 *   bootstrap flags then come from the constants below, which mirror
 *   `hs_config::reload::RELOADABLE_SECTIONS` and
 *   `hs_config::store::BOOTSTRAP_SECTIONS`.
 * - **Section metadata.** An array of objects, or an object keyed by name.
 * - **Per-setting metadata.** The `settings` array above, or the older
 *   `origins` map keyed by RFC 6901 JSON Pointer
 *   (`/auth/enable_registration`, what `hs_config::document::origins`
 *   produces), by the dotted path `hs-config`'s validation errors use
 *   (`auth.enable_registration`), or nested one object per level. All of them
 *   normalise to dotted paths, which is what the rest of the interface keys
 *   on.
 *
 * The request is a hand-rolled `fetch` rather than a typed `api.GET` because
 * the path only reached the OpenAPI document mid-flight; going through the
 * generated client would couple this module to whether the local
 * `src/api/schema.d.ts` has been regenerated. It still throws
 * {@link ApiProblemError} for an RFC 9457 body, so `QueryProblemState` treats
 * a 501/503 here exactly like every other call.
 */
import { apiBaseUrl } from "./client";
import { ApiProblemError, type Problem } from "./problem";
import { getAccessToken } from "@/lib/auth";

/** Any JSON value. The configuration document is JSON all the way down. */
export type JsonValue = null | boolean | number | string | JsonValue[] | { [k: string]: JsonValue };

/**
 * Where one setting's effective value came from, lowest precedence first —
 * `hs_config::document::Origin`. `environment` is above `database` on
 * purpose: a deployment that pins a setting in a Kubernetes manifest or a
 * systemd unit has said something the server will not quietly override, and
 * the API refuses to write it.
 */
export type ConfigOrigin = "default" | "file" | "database" | "environment";

export const CONFIG_ORIGINS: readonly ConfigOrigin[] = [
  "default",
  "file",
  "database",
  "environment",
];

/** Mirrors `hs_config::reload::RELOADABLE_SECTIONS`; the fallback when the API omits the flags. */
export const KNOWN_RELOADABLE_SECTIONS: readonly string[] = [
  "rate_limits",
  "federation",
  "telemetry",
  "appservices",
];

/** Mirrors `hs_config::store::BOOTSTRAP_SECTIONS`: read before the database is open. */
export const KNOWN_BOOTSTRAP_SECTIONS: readonly string[] = ["storage"];

/** Mirrors `hs_config::reload::SECTION_NAMES` — declaration order of the `Config` struct. */
export const KNOWN_SECTION_ORDER: readonly string[] = [
  "server",
  "listeners",
  "storage",
  "media",
  "federation",
  "rate_limits",
  "auth",
  "appservices",
  "telemetry",
  "cluster",
];

/** The JSON Schema keywords this interface renders from. Anything else is ignored, not rejected. */
export interface JsonSchemaNode {
  $ref?: string;
  $defs?: Record<string, JsonSchemaNode>;
  type?: string | string[];
  title?: string;
  description?: string;
  format?: string;
  default?: JsonValue;
  enum?: JsonValue[];
  const?: JsonValue;
  properties?: Record<string, JsonSchemaNode>;
  required?: string[];
  additionalProperties?: boolean | JsonSchemaNode;
  items?: JsonSchemaNode;
  oneOf?: JsonSchemaNode[];
  anyOf?: JsonSchemaNode[];
  allOf?: JsonSchemaNode[];
  minimum?: number;
  maximum?: number;
  writeOnly?: boolean;
  /** Extension: track 13 marks secret-valued fields so they render hidden even before a value arrives. */
  "x-secret"?: boolean;
  /** Extension: a per-setting origin carried on the node instead of the `origins` map. */
  "x-origin"?: ConfigOrigin;
}

export interface ConfigSectionMeta {
  name: string;
  /** Applies without a process restart. */
  reloadable: boolean;
  /** Read before the database is open, so it cannot be stored in it — read-only here. */
  bootstrap: boolean;
  /** The highest-precedence layer contributing to the section, when the server says. */
  source?: string;
}

/** What the server says about one setting, as it stands right now. */
export interface ConfigSettingInfo {
  origin: ConfigOrigin;
  /** The value is served redacted (`{"$secret": true}`). */
  secret: boolean;
  reloadable: boolean;
  /**
   * `config.update` would accept a change to it. False for a bootstrap
   * section and for anything an `HS__` variable pins — the server's own
   * answer, so the interface does not have to infer it from the origin.
   */
  editable: boolean;
}

export interface ConfigSchemaModel {
  /** The whole-config schema: `properties` has one entry per section. */
  root: JsonSchemaNode;
  /** `$defs`, for resolving `$ref`. */
  defs: Record<string, JsonSchemaNode>;
  sections: ConfigSectionMeta[];
  /** Per-setting metadata, keyed by dotted path (`auth.enable_registration`). */
  settings: Record<string, ConfigSettingInfo>;
  /** Origin per setting, keyed the same way — a projection of {@link settings}. */
  origins: Record<string, ConfigOrigin>;
  /** The configuration store's revision at the time this was read, when the server says. */
  revision?: number;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isOrigin(value: unknown): value is ConfigOrigin {
  return typeof value === "string" && (CONFIG_ORIGINS as readonly string[]).includes(value);
}

/**
 * `/auth/enable_registration` (RFC 6901) or `auth.enable_registration` → the dotted form.
 * Pointer tokens are unescaped per RFC 6901 (`~1` is `/`, `~0` is `~`); a key containing a literal
 * dot would be ambiguous in the dotted form, but configuration keys are `snake_case` Rust field
 * names, so that cannot arise today.
 */
export function toDottedPath(key: string): string {
  if (!key.startsWith("/")) return key;
  return key
    .slice(1)
    .split("/")
    .map((token) => token.replace(/~1/g, "/").replace(/~0/g, "~"))
    .join(".");
}

function collectOrigins(
  value: unknown,
  prefix: string,
  out: Record<string, ConfigOrigin>,
): Record<string, ConfigOrigin> {
  if (isOrigin(value)) {
    if (prefix) out[prefix] = value;
    return out;
  }
  if (!isRecord(value)) return out;
  for (const [key, child] of Object.entries(value)) {
    const dotted = toDottedPath(key);
    collectOrigins(child, prefix ? `${prefix}.${dotted}` : dotted, out);
  }
  return out;
}

function sectionMetaFrom(raw: unknown, name: string): ConfigSectionMeta {
  const record = isRecord(raw) ? raw : {};
  const reloadable =
    typeof record.reloadable === "boolean"
      ? record.reloadable
      : KNOWN_RELOADABLE_SECTIONS.includes(name);
  const bootstrap =
    typeof record.bootstrap === "boolean"
      ? record.bootstrap
      : KNOWN_BOOTSTRAP_SECTIONS.includes(name);
  return {
    name,
    reloadable,
    bootstrap,
    source: typeof record.source === "string" ? record.source : undefined,
  };
}

/** The `settings` array, keyed by dotted path. Unknown fields are left alone. */
function collectSettings(
  raw: unknown,
  sections: ConfigSectionMeta[],
): Record<string, ConfigSettingInfo> {
  if (!Array.isArray(raw)) return {};
  const bootstrapSections = new Set(sections.filter((s) => s.bootstrap).map((s) => s.name));
  const out: Record<string, ConfigSettingInfo> = {};
  for (const entry of raw) {
    if (!isRecord(entry) || typeof entry.pointer !== "string") continue;
    const path = toDottedPath(entry.pointer);
    if (!path) continue;
    const origin = isOrigin(entry.origin) ? entry.origin : "default";
    const section = typeof entry.section === "string" ? entry.section : path.split(".")[0];
    out[path] = {
      origin,
      secret: entry.secret === true,
      reloadable: entry.reloadable === true,
      // An older server that does not send `editable` gets the inference this
      // interface used before the field existed.
      editable:
        typeof entry.editable === "boolean"
          ? entry.editable
          : origin !== "environment" && !bootstrapSections.has(section),
    };
  }
  return out;
}

function orderSections(sections: ConfigSectionMeta[]): ConfigSectionMeta[] {
  const rank = (name: string) => {
    const index = KNOWN_SECTION_ORDER.indexOf(name);
    return index === -1 ? KNOWN_SECTION_ORDER.length : index;
  };
  return [...sections].sort((a, b) => rank(a.name) - rank(b.name) || a.name.localeCompare(b.name));
}

/** Turns whatever `GET /config/schema` answered into the model the rest of the interface uses. */
export function normalizeConfigSchema(raw: unknown): ConfigSchemaModel {
  const envelope = isRecord(raw) ? raw : {};
  // `{schema: {...}}`, or the schema document itself.
  const rootCandidate = isRecord(envelope.schema) ? envelope.schema : envelope;
  const root = rootCandidate as JsonSchemaNode;
  const defs = (root.$defs ?? {}) as Record<string, JsonSchemaNode>;

  const declared = envelope.sections;
  let sections: ConfigSectionMeta[];
  if (Array.isArray(declared)) {
    sections = declared
      .map((entry) => {
        const name = isRecord(entry) && typeof entry.name === "string" ? entry.name : "";
        return name ? sectionMetaFrom(entry, name) : null;
      })
      .filter((s): s is ConfigSectionMeta => s !== null);
  } else if (isRecord(declared)) {
    sections = Object.entries(declared).map(([name, entry]) => sectionMetaFrom(entry, name));
  } else {
    sections = Object.keys(root.properties ?? {}).map((name) => sectionMetaFrom(null, name));
  }

  const ordered = orderSections(sections);
  const settings = collectSettings(envelope.settings, ordered);

  // `origins` is the older spelling and still the fallback; entries from
  // `settings` win, since that is the shape the server actually ships.
  const origins = collectOrigins(envelope.origins ?? {}, "", {});
  // A node-carried `x-origin` is a supported alternative to both.
  for (const [sectionName, node] of Object.entries(root.properties ?? {})) {
    collectNodeOrigins(node, defs, sectionName, origins);
  }
  const bootstrapSections = new Set(ordered.filter((s) => s.bootstrap).map((s) => s.name));
  for (const [path, origin] of Object.entries(origins)) {
    if (path in settings) continue;
    settings[path] = {
      origin,
      secret: false,
      reloadable: false,
      editable: origin !== "environment" && !bootstrapSections.has(path.split(".")[0]),
    };
  }
  for (const [path, info] of Object.entries(settings)) {
    origins[path] = info.origin;
  }

  return {
    root,
    defs,
    sections: ordered,
    settings,
    origins,
    revision: typeof envelope.revision === "number" ? envelope.revision : undefined,
  };
}

function collectNodeOrigins(
  node: JsonSchemaNode,
  defs: Record<string, JsonSchemaNode>,
  path: string,
  out: Record<string, ConfigOrigin>,
  depth = 0,
): void {
  if (depth > 12) return;
  const resolved = resolveRef(node, defs);
  if (resolved["x-origin"] && !(path in out)) out[path] = resolved["x-origin"];
  for (const [key, child] of Object.entries(resolved.properties ?? {})) {
    collectNodeOrigins(child, defs, `${path}.${key}`, out, depth + 1);
  }
}

/** Follows a `$ref` into `$defs` once; returns the node unchanged when there is nothing to follow. */
export function resolveRef(
  node: JsonSchemaNode,
  defs: Record<string, JsonSchemaNode>,
  depth = 0,
): JsonSchemaNode {
  if (!node.$ref || depth > 8) return node;
  const name = node.$ref.replace(/^#\/(\$defs|definitions)\//, "");
  const target = defs[name];
  if (!target) return node;
  // A `$ref` sibling to `description`/`default` (schemars emits both for a
  // documented field) keeps the sibling: the field's own doc comment beats
  // the shared type's.
  const { $ref: _ref, ...rest } = node;
  return { ...resolveRef(target, defs, depth + 1), ...rest };
}

/** Fetches and normalises the schema. Throws {@link ApiProblemError} for an RFC 9457 body. */
export async function fetchConfigSchema(): Promise<ConfigSchemaModel> {
  const token = getAccessToken();
  const response = await fetch(`${apiBaseUrl()}/config/schema`, {
    headers: token ? { Authorization: `Bearer ${token}` } : undefined,
  });
  const body: unknown = await response.json().catch(() => null);
  if (!response.ok) {
    const parsed: Problem =
      isRecord(body) && typeof body.status === "number" && typeof body.title === "string"
        ? {
            type: typeof body.type === "string" ? body.type : "about:blank",
            title: body.title,
            status: body.status,
            detail: typeof body.detail === "string" ? body.detail : undefined,
            required_scope:
              typeof body.required_scope === "string" ? body.required_scope : undefined,
          }
        : {
            type: "about:blank",
            title: `Request failed with status ${response.status}`,
            status: response.status,
          };
    throw new ApiProblemError(parsed);
  }
  return normalizeConfigSchema(body);
}
