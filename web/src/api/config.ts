/**
 * Configuration (`/config`) — the resource behind the Configuration pages.
 *
 * Configuration lives in the server's database, not in a file on the host
 * (`crates/hs-config/src/store.rs`), which is what makes editing it from a
 * browser meaningful at all. Four things about the API shape the hooks here:
 *
 * - **Concurrency is an ETag.** `PATCH /config/{section}` takes `If-Match`
 *   carrying the section's revision, so two operators editing at once get a
 *   `412` rather than a silent last-writer-wins. The revision is not a field
 *   on `ConfigSection`, so {@link useConfigSection} keeps the response's
 *   `ETag` header alongside the body and {@link useUpdateConfigSection}
 *   sends it back.
 * - **The patch is RFC 7396.** Only what changed is sent, and `null` means
 *   "reset to the schema default" rather than "set to null" — see
 *   `crates/hs-config/src/document.rs`'s module doc, and
 *   `lib/config-model.ts`'s `buildMergePatch`.
 * - **Secrets are write-only.** They come back as `{"$secret": true}` and go
 *   out as a plain string; there is no endpoint that reveals one.
 * - **History is per setting, and revertible.** `GET /config/{section}/history`
 *   (`config.history.list`) lists every write to the section, one row per
 *   setting it touched with what the database held before and what the write
 *   left, secrets redacted on both sides ({@link useConfigHistory}).
 *   `POST /config/{section}/history/{revision}/revert` undoes one as a new
 *   revision ({@link useRevertConfigChange}); the server restores a secret from
 *   its own record, so the interface never holds one.
 *
 * `GET /config/schema` lives in `./config-schema.ts` — see that module's doc
 * comment for why it is a hand-rolled fetch rather than a typed call.
 */
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, newIdempotencyKey } from "./client";
import { fetchConfigSchema, type JsonValue } from "./config-schema";
import { unwrap } from "./problem";
import type { components } from "./schema";

export type AuditEntry = components["schemas"]["AuditEntry"];

/**
 * One side of a setting's change. `set: false` means the database held
 * nothing, so the setting read from the bootstrap file or the schema default.
 */
export interface ConfigSettingValue {
  set: boolean;
  /** When `set`. A secret is `{"$secret": true}`, never the secret. */
  value?: JsonValue;
}

/** One setting one change touched (`components.schemas.ConfigSettingChange`). */
export interface ConfigSettingChange {
  /** Whole-configuration JSON Pointer: `/rate_limits/login/per_second`. */
  pointer: string;
  /** The same, dotted: `rate_limits.login.per_second`. */
  path: string;
  /** The value is, or holds, a secret: both sides are redacted. */
  secret: boolean;
  /** `null` when the change predates the server keeping prior values. */
  from: ConfigSettingValue | null;
  to: ConfigSettingValue;
}

/** One recorded write to a section (`components.schemas.ConfigChange`). */
export interface ConfigChange {
  revision: number;
  section: string;
  actor: string | null;
  at: string;
  settings: ConfigSettingChange[];
  /** The revision this change reverted, when it was a revert. */
  reverts: number | null;
  /** Whether `config.history.revert` can undo it at all. */
  revertible: boolean;
}

export interface ConfigHistoryPage {
  items: ConfigChange[];
  next_cursor: string | null;
  prev_cursor: string | null;
}

function asConfigChange(raw: components["schemas"]["ConfigChange"]): ConfigChange {
  return {
    revision: raw.revision ?? 0,
    section: raw.section ?? "",
    actor: raw.actor ?? null,
    at: raw.at ?? "",
    settings: (raw.settings ?? []).map((row) => ({
      pointer: row.pointer,
      path: row.path,
      secret: row.secret,
      from: row.from ? { set: row.from.set, value: row.from.value as JsonValue | undefined } : null,
      to: { set: row.to.set, value: row.to.value as JsonValue | undefined },
    })),
    reverts: raw.reverts ?? null,
    revertible: raw.revertible ?? false,
  };
}

/**
 * `components["schemas"]["ConfigSection"]` with a usable `values`: the
 * OpenAPI document declares it as a bare `type: object`, which
 * `openapi-typescript` renders as `Record<string, never>` — a type nothing
 * can be read out of. The rest of the fields match the generated one.
 */
export interface ConfigSection {
  name: string;
  reloadable: boolean;
  source: string;
  last_reloaded_at?: string | null;
  values: Record<string, JsonValue>;
  /**
   * Only on the answer to a save (`config.update`): what the save did to the
   * running server. `null` from a server that does not say.
   */
  applied?: ConfigReloadReport | null;
}

export interface ConfigValidateReport {
  valid: boolean;
  errors: components["schemas"]["ValidationError"][];
  /** Sections whose new value could not be applied without restarting the process. */
  requires_restart: string[];
}

export interface ConfigReloadReport {
  /** Sections in which a changed setting was applied to the running server, now. */
  reloaded_sections: string[];
  /** A reloadable section the running server could not take on, at its pointer (`/rate_limits`). */
  errors: components["schemas"]["ValidationError"][];
  /** Sections holding a change that is only read at startup, so it waits for a restart. */
  requires_restart: string[];
}

/** A section plus the revision to send back as `If-Match`. */
export interface ConfigSectionWithEtag {
  section: ConfigSection;
  /** `null` when the server sent no `ETag`; the update then goes out unconditionally. */
  etag: string | null;
}

function asConfigSection(raw: unknown): ConfigSection {
  const record = (raw ?? {}) as Partial<ConfigSection>;
  return {
    name: record.name ?? "",
    reloadable: record.reloadable ?? false,
    source: record.source ?? "unknown",
    last_reloaded_at: record.last_reloaded_at ?? null,
    values: (record.values ?? {}) as Record<string, JsonValue>,
    applied: record.applied
      ? {
          reloaded_sections: record.applied.reloaded_sections ?? [],
          errors: record.applied.errors ?? [],
          requires_restart: record.applied.requires_restart ?? [],
        }
      : null,
  };
}

/**
 * What a save did, in a sentence: whether `section`'s change is in force now,
 * waits for a restart, or both. Read from the server's own answer when it
 * gives one; otherwise inferred from whether the change was hot.
 */
export function describeApplied(
  applied: ConfigReloadReport | null | undefined,
  section: string,
  hotWithoutAnswer: boolean,
): { description: string; failed: boolean } {
  if (!applied) {
    return {
      description: hotWithoutAnswer
        ? "Applied to the running server."
        : "Stored. It takes effect the next time this server restarts.",
      failed: false,
    };
  }
  const failure = applied.errors.find((e) => e.pointer === `/${section}`);
  if (failure) {
    return {
      description: `Stored, but the running server could not take it on and keeps the old value: ${failure.detail}`,
      failed: true,
    };
  }
  const now = applied.reloaded_sections.includes(section);
  const later = applied.requires_restart.includes(section);
  const description =
    now && later
      ? "Part of it applies to the running server now; the rest takes effect the next time this server restarts."
      : later
        ? "Stored. It takes effect the next time this server restarts."
        : now
          ? "Applied to the running server."
          : "Stored. The running server already uses these values.";
  return { description, failed: false };
}

export function useConfigSections() {
  return useQuery({
    queryKey: ["config-sections"],
    queryFn: async () => {
      const result = await api.GET("/config");
      return unwrap(result).map(asConfigSection);
    },
  });
}

export function useConfigSection(section: string | undefined) {
  return useQuery({
    queryKey: ["config-section", section],
    enabled: Boolean(section),
    queryFn: async (): Promise<ConfigSectionWithEtag> => {
      const result = await api.GET("/config/{section}", {
        params: { path: { section: section! } },
      });
      const data = unwrap(result);
      return { section: asConfigSection(data), etag: result.response.headers.get("ETag") };
    },
  });
}

/** The whole-config JSON Schema, plus per-section flags and per-setting origins. */
export function useConfigSchema() {
  return useQuery({
    queryKey: ["config-schema"],
    queryFn: fetchConfigSchema,
    // The schema changes when the binary changes, not while a page is open.
    staleTime: 5 * 60_000,
  });
}

export interface UpdateConfigSectionInput {
  section: string;
  /** An RFC 7396 merge patch — `lib/config-model.ts`'s `buildMergePatch`. */
  patch: Record<string, JsonValue>;
  etag: string | null;
}

export function useUpdateConfigSection() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async ({ section, patch, etag }: UpdateConfigSectionInput) => {
      const result = await api.PATCH("/config/{section}", {
        params: {
          path: { section },
          header: etag ? { "If-Match": etag } : undefined,
        },
        body: patch as unknown as Record<string, never>,
      });
      const data = unwrap(result);
      return { section: asConfigSection(data), etag: result.response.headers.get("ETag") };
    },
    onSuccess: (_data, { section }) => {
      qc.invalidateQueries({ queryKey: ["config-sections"] });
      qc.invalidateQueries({ queryKey: ["config-section", section] });
      qc.invalidateQueries({ queryKey: ["config-schema"] });
      qc.invalidateQueries({ queryKey: ["config-history", section] });
    },
  });
}

/**
 * Checks a candidate configuration without applying it. The body is a whole
 * configuration document (the operation is `config.validate`, not
 * `config.validate_section`), so callers send `{[section]: candidate}` — the
 * one section they are editing, merged with their edits.
 */
export function useValidateConfig() {
  return useMutation({
    mutationFn: async (document: Record<string, JsonValue>): Promise<ConfigValidateReport> => {
      const result = await api.POST("/config/validate", {
        body: document as unknown as Record<string, never>,
      });
      const data = unwrap(result);
      return {
        valid: data.valid ?? false,
        errors: data.errors ?? [],
        requires_restart: data.requires_restart ?? [],
      };
    },
  });
}

/** Re-reads the bootstrap file and hot-applies every reloadable section. */
export function useReloadConfig() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (): Promise<ConfigReloadReport> => {
      const result = await api.POST("/config/reload", {
        params: { header: { "Idempotency-Key": newIdempotencyKey() } },
      });
      const data = unwrap(result);
      return {
        reloaded_sections: data.reloaded_sections ?? [],
        errors: data.errors ?? [],
        requires_restart: data.requires_restart ?? [],
      };
    },
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["config-sections"] });
      qc.invalidateQueries({ queryKey: ["config-section"] });
    },
  });
}

/**
 * One page of a section's history, newest first. `cursor` is the page's
 * `next_cursor`/`prev_cursor` from the one before (the page keeps it in the
 * URL, so a page of history is a link).
 */
export function useConfigHistory(section: string | undefined, cursor?: string, limit = 10) {
  return useQuery({
    queryKey: ["config-history", section, cursor ?? null, limit],
    enabled: Boolean(section),
    queryFn: async (): Promise<ConfigHistoryPage> => {
      const result = await api.GET("/config/{section}/history", {
        params: { path: { section: section! }, query: { limit, cursor } },
      });
      const data = unwrap(result);
      return {
        items: (data.items ?? []).map(asConfigChange),
        next_cursor: data.next_cursor ?? null,
        prev_cursor: data.prev_cursor ?? null,
      };
    },
  });
}

export interface RevertConfigChangeInput {
  section: string;
  revision: number;
  /** The section's `ETag`, so a revert computed against a stale page is refused (`412`). */
  etag: string | null;
  /** Go ahead although later changes wrote the same settings (undoing them too). */
  force?: boolean;
}

/**
 * Undoes one change as a new revision. A `409` means later changes wrote some
 * of the same settings — the problem's `errors[]` names them — and the caller
 * may ask again with `force`.
 */
export function useRevertConfigChange() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async ({ section, revision, etag, force }: RevertConfigChangeInput) => {
      const result = await api.POST("/config/{section}/history/{revision}/revert", {
        params: {
          path: { section, revision },
          header: etag ? { "If-Match": etag } : undefined,
        },
        body: { force: Boolean(force) },
      });
      const data = unwrap(result);
      return { section: asConfigSection(data), etag: result.response.headers.get("ETag") };
    },
    onSettled: (_data, _error, { section }) => {
      qc.invalidateQueries({ queryKey: ["config-sections"] });
      qc.invalidateQueries({ queryKey: ["config-section", section] });
      qc.invalidateQueries({ queryKey: ["config-schema"] });
      qc.invalidateQueries({ queryKey: ["config-history", section] });
    },
  });
}
