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
 * - **Change history is the audit log.** There is no `/config/history`
 *   operation in `crates/hs-admin/openapi/openapi.yaml`; the audit log is
 *   where a configuration write is recorded, and `config_section` is one of
 *   the resource types it names. {@link useConfigHistory} filters it.
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
}

export interface ConfigValidateReport {
  valid: boolean;
  errors: components["schemas"]["ValidationError"][];
  /** Sections whose new value could not be applied without restarting the process. */
  requires_restart: string[];
}

export interface ConfigReloadReport {
  reloaded_sections: string[];
  errors: components["schemas"]["ValidationError"][];
  /** Sections the reload could not apply, because they are not reloadable. */
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
  };
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

/** Who changed this section, when — the audit log, filtered to one config section. */
export function useConfigHistory(section: string | undefined, limit = 20) {
  return useQuery({
    queryKey: ["config-history", section, limit],
    enabled: Boolean(section),
    queryFn: async () => {
      const result = await api.GET("/audit-log", {
        params: { query: { target_type: "config_section", target_id: section!, limit } },
      });
      return unwrap(result).items as AuditEntry[];
    },
  });
}
